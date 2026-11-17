# flow — tábla ↔ property megfeleltetés és pluginok

A `callgraph flow map` a PHP-osztályokból és a pluginokból épített megfeleltetést írja ki: melyik osztály melyik
property-je melyik tábla melyik oszlopa. Ez a későbbi adatfolyam-gráf (DB-érték → use case olvasás) bemenete.

**SQL-t nem parse-olunk.** Az oszlopnév a PHP-kódból jön: `$fieldsMap` literál tömb, kézi szabály, vagy plugin.
A `SELECT …` szöveg elemzése külön feladat.

```bash
bash ~/.claude/skills/callgraph/callgraph.sh flow map --project raketa
bash ~/.claude/skills/callgraph/callgraph.sh flow map --project raketa --table cmn_hotel_chain
bash ~/.claude/skills/callgraph/callgraph.sh flow map --project raketa --json
```

```
root: /projects/raketa/raketa/core
flow map: 12 tables · 318 columns · 54 classes · legacy-fields-map 300, manual 18
cmn_hotel_chain
  name_en  ←  Common\HotelChain::$nameEn  (legacy-fields-map)
```

## Konfiguráció: `.callgraph-flow.json`

A projekt gyökerében (JSONC: `//` és `/* */` megjegyzés is lehet). A fájl nélkül a beépített
`legacy-fields-map` handler fut az alapértelmezett nevekkel.

```jsonc
{
  // kézi felülírás — mindig a legmagasabb prioritás
  "map": [
    { "class": "Common\\HotelChain", "property": "nameEn", "table": "cmn_hotel_chain", "column": "name_en" }
  ],

  // beépített handlerek beállításai
  "handlers": [
    {
      "id": "legacy-fields-map",
      "extends": "Billet",          // csak a Billet leszármazottakra
      "tableProperty": "tableName",  // $tableName = '…'
      "mapProperty": "fieldsMap"     // $fieldsMap = ['property' => 'column', …]
    }
  ],

  // külső pluginok (lásd lentebb)
  "plugins": [
    { "id": "billet-legacy", "command": "php tools/callgraph-flow-billet.php" }
  ]
}
```

A `fieldsMap` sorrendjét a deklarált property-k döntik el: ha a kulcs property-név, akkor `property => column`; ha az
érték az, akkor `column => property`; egyébként `property => column` (ez a `Billet` konvenciója).

## `flow trace`: hol olvassák az értéket

A `flow map` csak a DB ↔ property éleket adja. A `flow trace` ugyanazon a tényeken (és a pluginokon) végigmegy a
kódon: metóduson belül követi a változókat, property-ket és tömbkulcsokat, metódusok között a `return` / paraméter /
property éleken, és kiírja, hol olvassák az értéket (nem a hozzárendelést: az csak továbbadás).

```bash
callgraph trace service_type --project raketa
callgraph trace service_type record_type --project raketa --json
```

```
root: /projects/raketa/raketa/api/queue
trace: service_type · 3 reads in 3 files
v1/src/Operation/GetQueueOperation.php  Api\Queue\V1\Operation\GetQueueOperation::mappingResponse  L64
  $queue->getServiceType()
  key:service_type → p:Api\Queue\Entity\Queue::__construct#2 → p:Api\Queue\Entity\QueueNoId::__construct#1 → prop:Api\Queue\Entity\QueueNoId::serviceType → c:Api\Queue\Entity\Queue::getServiceType
```

Korlátok: nincs SQL-parse (a kulcs a PHP-ból jön), dinamikus hívás esetén a lánc megszakad (`unknown`); a `__get` /
`extract` / `fetchObject` eseteket may-only heurisztikák fedik (lásd lentebb). A fixpont flow-érzéketlen (egy változó
egy metódusban egy irokészlet). A `c:` visszatérés
híváspont-pontos: a `p:` paramétert az adott hívás argumentumai helyettesítik, így egy literál argumentumú hívás nem
örökli más hívások oszlopát. A gettereken át olvasott `prop:` a létrehozási helyhez kötődik (konstruktor-argumentumok,
`parent::__construct` forwarding, paraméteren át kapott objektum), a setter-írások és a `$this->m()` lánc pedig a hívás
helyének fogadójához (call-site kontextus); ismeretlen fogadó esetén globális fallback. Kivétel: egy `$this`-láncon
belüli `$this->setX()` továbbra is globális.

### may / must

A `[must]` jelölés azt jelenti: a kolónia **minden olyan úton** eljut a reading, amely eljut magához a readinghez.
`--must` csak ezeket listázza; alapból may („legalább egy úton”).

Az elemzés path-sensitive az `if` / `elseif` / `else`, a ternáris operátor, a `switch` és a `match` esetén, valamint
korai `return` / `break` / `continue` mellett. A szűkítés egységes: `&&` / `||` / `!`, `$flag = true|false`, `$x === null`,
`isset($x)`, `isset($a['k'])`, `array_key_exists('k', $a)`, `$x instanceof Foo`; ellentmondó ágakat kihagy.
A ciklusok legfeljebb 3 menetes fixpontig futnak, és csak a testben írt változókat általánosítják; a `do-while` teste
mindig lefut, ezért ott a `must` megmaradhat. A `switch` valódi fallthrough-t és `break`-et modellez; az „egyik case sem
egyezett” út csak `default` nélkül marad meg. A `match` áganként külön scope-ban fut, az érték may uniója az ágaké,
`must` csak `default` és azonos must-halmazok esetén. A property-írások `must`-ja továbbra is csak a konstruktorból
számol, ezért a getteren át olvasott érték lehet may annak ellenére, hogy a konstruktor mindig beállítja.

## Hydration: osztály kitöltése kód nélkül

JsonMapper / Symfony Serializer / Doctrine a property-ket a PHP AST-n kívül tölti ki. Erre való a `hydrate` szabály:
osztály + tábla + property → oszlop. Nem kell, hogy az osztály szerepeljen a forrásban.

```jsonc
{
  "hydrate": [
    { "class": "App\\Dto\\Hotel", "table": "hotel", "properties": { "nameEn": "name_en" } }
  ]
}
```

A `trace` ebből `sql:<tábla>.<oszlop>` atomot készít, így a `trace hotel.name_en` megtalálja a `$dto->nameEn`
olvasást akkor is, ha a property-t sosem írja PHP-kód. Prioritás: handler → plugin → hydrate → kézi `map`.

## Mit lát a trace

- `trace <oszlop>`: minden olvasás, ahol a kulcs/property/kódott oszlop megjelenik (may), `[must]` a minden úton
  eljutó olvasásokon.
- `trace <tábla>.<oszlop>`: a tábla is szűr (a `flow map` / `hydrate` megfeleltetésből); enélkül az `id`/`del`
  típusú nevek minden táblát összemosnak.
- Interfészen át hívott metódus: a visszatérés az összes implementációból unió (may), must csak ha minden
  implementáció garantálja; trait-metódus a használó osztályon keresztül oldódik fel.
- Legacy konstrukciók: `try`/`catch`/`finally`, `list()` / `[...]` destructuring, `yield` / `yield from`,
  `array_column($rows, 'oszlop')`, statikus `call_user_func([$this,'m'], …)`.

## Recall heurisztikák (may-only)

A bizonytalan olvasások felderítésére a trace közelítő szabályokat is használ. Ezek **csak may élt** adnak, must-ot
soha:

- ismeretlen fogadójú property-olvasás (`$row->name_en` `fetchObject` után) → `key:name_en`;
- `__get` / `__set`-es osztályon a property-olvasás a property nevét is hordozza: `key:<prop>`;
- `extract($row)` → az ismeretlen változó olvasása `key:<változó>`;
- `get_object_vars($this)` → a `prop:this::*` uniója;
- `compact('a', 'b')` → a változók eredeteinek uniója;
- `match` értéke → az ágak eredeteinek uniója (a fogyasztó a kolónát látja);
- `switch` fallthrough → sorrendben feldolgozva;
- tömb pass-through (`array_values`, `array_filter`, `array_merge`, `array_slice`, `array_reverse`, `array_unique`,
  `array_diff`, `array_intersect`, `array_splice`) → a tömbargumentumok uniója;
- `array_map` → a callback visszatérési eredetei (arrow kifejezés, closure `return`, callable `c:` atom);
- named argumentumok és `...$spread` → a konstruktor / függvény paramétereihez rendelve;
- statikus property-k (`self::$x`) és `global` változók → hordozzák az eredetet;
- `foreach` kulcs literálhoz hasonlítva → `key:<literál>`.

Mivel ezek may-only közelítések, hamis pozitívumot adhatnak: pl. a `$row->name_en` nem feltétlenül DB-oszlop. A zajt a
`--must` (csak a minden úton eljutó olvasások) és a `<tábla>.<oszlop>` szűrő (csak az adott tábla oszlopa) csökkenti.

## További korlátok

- `extract($row)`, `fetchObject()` + `$row->oszlop`, `__get`/`__set`, `get_object_vars`, `foreach ($row as $k => $v)`
  kulcsneve: csak may heurisztikával követett, plugin/manual map továbbra is pontosabb.
- A `prop:` `must` csak a konstruktor-írásokból számol; setterek must-ja konzervatív.
- A `?? null` literál `lit` atomja nem különül el a többi literáltól, ezért `$x = $row['k'] ?? null; if ($x !== null)`
  után a `null`-ág eltávolítása nem teheti `must`-tá a `key:`-t (a PHPStan-szerű conditional holder ehhez még hiányzik).
- A `switch` exhaustiveness csak `default`-ból jön; enum-/literál-domén kimerülését nem számoljuk.
- A `match` `must`-ja csak teljesen azonos ág-must-halmazoknál adódik (szigorú konzervatív szabály).

## SQL-parsing (MySQL 8.4)

A `flow` a `fetch*` / `query*` / `execute*` hívások első argumentumából (literál vagy ismert string-változó,
heredoc is) kiolvassa a SELECT-et `sqlparser`-rel (`MySqlDialect`), és a `$row['oszlop']` olvasást
`sql:<tábla>.<oszlop>` atommá köti. Így a `trace <tábla>.<oszlop>` akkor is működik, ha nincs `.callgraph-flow.json`
mapping (a séma/backtick levágva, az aliasok azonos nevűek).

```
trace: api_queue.queue.service_type · 3 reads in 3 files
src/Repository/QueueRepository.php  ...::makeEntity  L270
  $row['service_type']
  sql:api_queue.queue.service_type → key:service_type → row:api_queue.queue
```

Korlátok: dinamikus SQL (`{$x}`, `%s`, `VALUES $x`, `IN :ids`) előfeldolgozás után is sokszor törhető → ilyenkor
csak `key:` marad; `SELECT *` esetén a tábla ismert, a kolónia nem; alias (`AS st`) és CTE/derived table
feloldása még hiányos; `sqlparser` szintaxis, nem séma. A core teljes scanje ~2 perc (a `trace` opt-in parancs),
a queue ~0,1 s.

## Külső plugin (bármilyen nyelven)

A parancs **egyszer** fut le projektenként: a stdin-en megkapja az összes osztály JSON-ját, a stdout-ra a
megfeleltetést írja. Nem kell újrafordítani a `callgraph`-ot.

Kérés (stdin):

```json
{
  "schema": 1,
  "classes": [
    {
      "file": "core/src/Common/HotelChain.php",
      "name": "Common\\HotelChain",
      "short": "HotelChain",
      "extends": ["Billet"],
      "properties": [
        { "name": "tableName", "ty": null, "default": "cmn_hotel_chain", "promoted": false },
        { "name": "fieldsMap", "ty": null, "default": [["nameEn", "name_en"]], "promoted": false }
      ]
    }
  ]
}
```

Válasz (stdout):

```json
{
  "columns": [
    { "class": "Common\\HotelChain", "property": "nameEn", "table": "cmn_hotel_chain", "column": "name_en" }
  ]
}
```

Példa handler (`tools/callgraph-flow-billet.php`):

```php
<?php
$request = json_decode(stream_get_contents(STDIN), true);
$columns = [];
foreach ($request['classes'] as $class) {
    $props = array_column($class['properties'], 'default', 'name');
    $table = $props['tableName'] ?? null;
    if (!is_string($table)) {
        continue;
    }
    foreach ($props['fieldsMap'] ?? [] as [$property, $column]) {
        $columns[] = compact('table', 'column') + ['class' => $class['name'], 'property' => $property];
    }
}
echo json_encode(['columns' => $columns]);
```

A plugin a beépített handlerek után fut (felülírja azokat), a kézi `map` szabályok viszont a plugin felett állnak.
Hibás plugin (nem indul el, hibás JSON) nem állítja meg a futást: `⚠ plugin <id>: …` figyelmeztetés a stderr-en.

## Saját, fordított handler

A handler egy szűk interfészt valósít meg, és egyetlen tényt ad vissza (mint a PHPStan extensionjei):
tábla- vagy oszlop-megfeleltetést.

```rust
use callgraph::flow::{ClassFacts, Mapping, PropertyColumnHandler};

struct MyHandler;

impl PropertyColumnHandler for MyHandler {
    fn id(&self) -> &'static str { "my-handler" }
    fn columns(&self, class: &ClassFacts, out: &mut Mapping) {
        // pl. egy konvenció: `*Entity` osztály property-je `snake_case` oszlop
        if class.short.ends_with("Entity") {
            for p in &class.properties {
                out.insert(&class.name, &p.name, "unknown", &snake_case(&p.name), self.id());
            }
        }
    }
}
```

Táblára ugyanez `ClassTableHandler`-rel (`fn table(...)`). Regisztrálás:
`Registry::from_config(config).with_column_handler(Box::new(MyHandler))`, illetve `with_table_handler(...)`.

## Korlátok

- Nem literál `$fieldsMap` / `$tableName` (változó, hívás, konstans): a handler kihagyja — ilyenkor kézi `map` vagy plugin.
- `__get` / `__set` / `__call` által kezelt property: csak akkor van megfeleltetés, ha a handler vagy a `map` megadja.
- Dinamikus tábla-/oszlopnév (`$this->tableName = $x`, `"t_{$suffix}"`): plugin dolga.
- A megfeleltetés önmagában még nem adatfolyam: a `flow map` csak a DB ↔ property éleket mutatja. A DB-érték →
  use case olvasás lánc a következő lépés (ugyanezen a `Mapping`-en).
