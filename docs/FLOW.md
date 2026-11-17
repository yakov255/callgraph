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

Korlátok: nincs SQL-parse (a kulcs a PHP-ból jön), dinamikus hívás / `__get` / `extract` esetén a lánc megszakad
(`unknown`), a fixpont flow-érzéketlen (egy változó egy metódusban egy irokészlet), és a `c:` visszatérési érték
minden hívási hely paramétereit egyesíti — ugyanaz a függvény literál argumentummal is örökölheti egy másik hívás
oszlopát (híváspont-pontos visszatérés: következő lépés).

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

```rust
use callgraph::flow::{ClassFacts, FlowHandler, Mapping};

struct MyHandler;

impl FlowHandler for MyHandler {
    fn id(&self) -> &'static str { "my-handler" }
    fn apply(&self, class: &ClassFacts, out: &mut Mapping) {
        // pl. egy konvenció: `*Entity` osztály property-je `snake_case` oszlop
        if class.short.ends_with("Entity") {
            for p in &class.properties {
                out.insert(&class.name, &p.name, "unknown", &snake_case(&p.name), self.id());
            }
        }
    }
}
```

Regisztrálás: `Registry::from_config(config).with_handler(Box::new(MyHandler))` (a `src/flow/plugin.rs`-ban, a
`from_config` végén egy sor).

## Korlátok

- Nem literál `$fieldsMap` / `$tableName` (változó, hívás, konstans): a handler kihagyja — ilyenkor kézi `map` vagy plugin.
- `__get` / `__set` / `__call` által kezelt property: csak akkor van megfeleltetés, ha a handler vagy a `map` megadja.
- Dinamikus tábla-/oszlopnév (`$this->tableName = $x`, `"t_{$suffix}"`): plugin dolga.
- A megfeleltetés önmagában még nem adatfolyam: a `flow map` csak a DB ↔ property éleket mutatja. A DB-érték →
  use case olvasás lánc a következő lépés (ugyanezen a `Mapping`-en).
