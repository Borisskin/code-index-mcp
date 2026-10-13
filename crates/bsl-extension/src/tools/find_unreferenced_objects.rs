// MCP-tool `find_unreferenced_objects` — объекты метаданных 1С, на которые не
// ссылается НИ ОДИН из проиндексированных каналов ссылок. Отвечает на вопрос
// «что в конфигурации не используется» (макеты, подсистемы, языки и т.п.),
// объединяя пять источников ссылок вместо серии запросов через `bsl_sql`.
//
// Формула (formula_version = "c2-v3"): объект — кандидат, если по нему нет ни
// одной ссылки ни в одном канале:
//
// v2 отличается от v1 (c2-v1) НАБОРОМ КАНАЛОВ: добавлены обращения к общим
// картинкам (`common_picture`: `БиблиотекаКартинок.X`/`CommonPicture.X`, в т.ч.
// из XML выгрузки), к элементам стиля (`style_item`: `ЦветаСтиля.X`/
// `ШрифтыСтиля.X` в коде и `style:X` в XML) и канал общих модулей (вызовы
// `Модуль.Метод()` по рёбрам `proc_call_graph`; self-рёбра модуля
// использованием не считаются). Числа сравнимы только внутри одной версии.
//
// v3 отличается от v2 ИСТОЧНИКАМИ канала общих модулей (числа те же по смыслу,
// но меньше кандидатов): к вызовам `Модуль.Метод()` из графа добавлены
//   * модули-обработчики подписок на события
//     (`event_subscriptions.handler_module` → модуль в `metadata_modules` →
//     `common_modules_via_subscription`): рёбра подписок резолвятся в ИМЯ
//     процедуры, поэтому у них `callee_proc_key IS NULL` и канал графа их не
//     видит вовсе;
//   * строковый доступ к модулю `ОбщийМодуль("Имя")`
//     (`common_module_via_literal`, `common_modules_via_literal`): обращение к
//     самому объекту `CommonModule`, а не вызов `Модуль.Метод()`;
//   * формы, которые по-прежнему НЕ индексируются, названы в
//     `NOT_INDEXED_COMMON_MODULE_FORMS` (см. ниже).
// Числа сравнимы только внутри одной версии.
//
//   * `data_links.to_object_key` — структурные ссылки из метаданных
//     (реквизиты/измерения/движения/владельцы/состав подсистем и т.п.);
//   * `role_rights.object_name_key` — права ролей на объект;
//   * `metadata_code_usages.object_ref_key` — прямые обращения в коде
//     (`manager` / `ref_type` / `query`), а также обращения к общим картинкам
//     (`common_picture`: `БиблиотекаКартинок.X`/`CommonPicture.X`, в т.ч. из
//     XML форм) и к элементам стиля (`style_item`: `ЦветаСтиля.X`/`style:X`);
//   * канал общих модулей: объект `CommonModule.X` считается использованным,
//     если он адресован извне хотя бы одним из двух источников:
//       a) вызов `Модуль.Метод()` — в `proc_call_graph` есть ребро на его
//          модуль (резолвер уже проставил
//          `callee_proc_key = <путь модуля X>::<метод>`
//          (`metadata_modules.code_path`) — переиндексация не нужна);
//          self-рёбра модуля использованием не считаются; счётчик
//          `common_modules`;
//       b) строка `ОбщийМодуль("X")` (`usage_kind = common_module_via_literal`)
//          — строковый доступ к самому объекту модуля; счётчик
//          `common_modules_via_literal`;
//       и, независимо от графа, как обработчик подписки на событие:
//       c) `event_subscriptions.handler_module = CommonModule.X` → модуль X
//          есть в `metadata_modules` (непустой `code_path`) — платформа зовёт
//          его сама, ребра вызова у подписки нет (`callee_proc_key IS NULL`,
//          в нём лежит имя процедуры); счётчик
//          `common_modules_via_subscription`;
//   * канал макетов, четыре источника (Требование 2 ТЗ):
//       a) строковые пути метаданных (`query`-строки с `member_path`) — пара
//          (ключ владельца, имя макета): например `"Справочник.Вакансии.ПФ_MXL_Вакансия"`;
//       b) `template_call` — `ПолучитьМакет("Имя")`; владелец резолвится по
//          `file_path` вызывающего модуля: сначала
//          `metadata_modules.code_path → object_name` (папка выгрузки →
//          канонический тип, у модулей форм/команд владелец — сам объект),
//          затем фолбэк — разбор пути `<ПапкаВыгрузки>/<Имя>/…`; если ничего
//          не сработало, вызов считается нерезолвленным
//          (`template_call_sites_unresolved`);
//       c) `common_template_call` — `ПолучитьОбщийМакет("Имя")` →
//          `CommonTemplate.<Имя>`;
//       d) `template_call_via_manager` — метод-форма
//          `Справочники.X.ПолучитьМакет("Y")`: владелец (ключ `catalog.x`) уже
//          лежит в `object_ref_key`, резолв по файлу не нужен. Места, где цель
//          `<владелец>.Template.<Y>` отсутствует в реестре объектов, считаются
//          отдельно (`template_call_sites_missing_target` — «цель вне реестра»,
//          в отличие от `template_call_sites_unresolved` — «владелец не выведен»).
//
// Форма XML-ссылок на общие картинки (`CommonPicture.X`) и элементы стиля
// (`style:X`) индексируется для ВСЕХ `.xml`, кроме `ConfigDumpInfo.xml` (это
// опись состава — перечень имён, не ссылки).
//
// Формы вызова макетов, которые НЕ индексируются, честно перечислены в
// `channels_not_applied` (Требование 0): `template_call_via_receiver`
// (метод-форма с нераспознанным приёмником — считается в
// `template_call_sites_uncovered`), `template_call_non_literal` (аргумент —
// переменная/склейка/второй аргумент), `template_call_english`
// (`GetTemplate`/`GetCommonTemplate`), `template_call_multiline` (вызов с
// переносом строки до литерала). Если строк вида `template_call*` в базе нет
// (вызовов `ПолучитьМакет` нет ЛИБО индекс собран прежним бинарником — из
// данных неразличимо), добавляется `template_calls_not_indexed` и оговорка в
// `hint`: полнота канала макетов не подтверждена (B2). Тем же правилом для
// каналов картинок/стилей добавляются `common_picture_usages_not_indexed` /
// `style_item_usages_not_indexed` / `common_module_literal_usages_not_indexed`
// (последний — строки вида `common_module_via_literal`), а для канала общих
// модулей — `common_module_calls_not_indexed` (нет адресов вызовов/строк
// модулей).
//
// Часть форм вызова общих модулей не индексируется ВООБЩЕ и всегда перечислена
// в `channels_not_applied` (Требование 0):
//   * запуск фонового задания по ИМЕНИ метода в литерале —
//     `ФоновыеЗадания.Выполнить("ОбщийМодуль.Метод", …)`
//     (`common_module_calls_via_background_job`);
//   * строковый реестр модулей библиотеки/подсистемы (БСП) —
//     `МодулиПодсистем.Добавить("ИмяОбщегоМодуля")`: имя цели задано строкой в
//     СПИСКЕ, а не вызовом `Модуль.Метод()` (`common_module_calls_via_library_registry`);
//   * квалифицированная цель-литерал в описании оповещения/обработчике
//     ожидания — `ОписаниеОповещения("ОбщийМодуль.Метод", …)`,
//     `ПодключитьОбработчикОжидания("ОбщийМодуль.Метод", …)`
//     (`common_module_calls_via_qualified_literal`);
//   * `ОбщийМодуль(Выражение)` (имя модуля не литерал) и
//     квалифицированная/метод-форма `ОбщегоНазначения.ОбщийМодуль("X")` —
//     обращение к объекту `CommonModule` по этим строкам не извлекается
//     (`common_module_via_qualified_or_expression`).
// Цель во всех этих формах задана строкой, а не вызовом `Модуль.Метод()`,
// поэтому ребра `proc_call_graph` для неё нет, и общий модуль может попасть в
// `results`, хотя используется. HTTPService/WebService-модули при этом В ГРАФЕ
// участвуют как вызыватели (их отличия от CommonModule нет) — видна только
// непокрытая форма вызова, а не тип модуля.
//
// `Subsystem`/`Language` ссылочно НЕ адресуемы по устройству (на подсистему и
// язык нельзя сослаться ни из кода, ни из метаданных), поэтому они исключены
// из `results` и отдаются счётчиком `not_addressable`.
//
// Ответ — под ключом `results` (не `result` — чтобы не сталкиваться с
// сессионным дедупом; см. Требование 0 ТЗ), страницами по бюджету ответа.
// `channels_applied` показывает, сколько объектов снял каждый канал (каналы
// пересекаются, поэтому сумма может быть больше числа снятых), `formula_version`
// фиксирует формулу. При устаревшем индексе (`stale`) числа не отдаются
// (`results`/`results_total` = null). Это НЕ доказательство мёртвости:
// `results` — кандидаты.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use code_index_core::extension::{IndexTool, ToolContext};
use code_index_core::mcp::cap;
use rusqlite::{params, params_from_iter};
use serde_json::{json, Map, Value};

/// Таймаут набора запросов (как в find_references/bsl_sql): по истечении
/// sqlite3_interrupt обрывает перебор на больших таблицах.
const QUERY_TIMEOUT_SECS: u64 = 8;

/// Версия формулы «без ссылок» — меняется при смене набора каналов, чтобы
/// числа сравнивались только внутри одной версии (Требование 0 ТЗ).
/// v3: канал общих модулей получил источники `event_subscriptions` и
/// `ОбщийМодуль("Имя")` (см. шапку файла).
const FORMULA_VERSION: &str = "c2-v3";

/// Типы, которые не бывают целями ссылок по устройству платформы: объекты
/// возвращаются в ответе, но не в `results`, а счётчиком `not_addressable`.
const NOT_ADDRESSABLE: &[&str] = &["Subsystem", "Language"];

/// Формы вызова макетов, которые НЕ индексируются вовсе (либо индексируются
/// только счётчиком): честно перечисляются в `channels_not_applied`
/// (Требование 0/2 ТЗ). Порядок — по каналу.
const NOT_INDEXED_TEMPLATE_FORMS: &[&str] = &[
    "template_call_via_receiver",
    "template_call_non_literal",
    "template_call_english",
    "template_call_multiline",
];

/// Формы обращения к общим модулям, которые НЕ индексируются ВООБЩЕ и всегда
/// перечислены в `channels_not_applied` (Требование 0): цель задана СТРОКОЙ, а
/// не вызовом `Модуль.Метод()`, поэтому ребра `proc_call_graph` для неё нет, и
/// общий модуль может попасть в `results`, хотя используется. Формы не
/// индексируются ни на каком индексе (структурный пропуск), поэтому входят в
/// список всегда.
const NOT_INDEXED_COMMON_MODULE_FORMS: &[&str] = &[
    // `ФоновыеЗадания.Выполнить("ОбщийМодуль.Метод", …)` — имя метода в литерале.
    "common_module_calls_via_background_job",
    // Строковый реестр модулей библиотеки/подсистемы (БСП):
    // `МодулиПодсистем.Добавить("ИмяОбщегоМодуля")` — платформа/БСП достаёт
    // модуль по имени из СПИСКА (`ОбщегоНазначения.ОбщийМодуль(Имя)` с
    // ПЕРЕМЕННОЙ), вызова `Модуль.Метод()` в коде для цели нет. Проверено на
    // выгрузках prod3 (объект `CommonModule.ОбновлениеИнформационнойБазыПроизводство`
    // адресован именно так) и ЗУП.
    "common_module_calls_via_library_registry",
    // Квалифицированная цель-литерал в описании оповещения/обработчике ожидания:
    // `ОписаниеОповещения("ОбщийМодуль.Метод", …)`,
    // `ПодключитьОбработчикОжидания("ОбщийМодуль.Метод", …)`. Индексер эти
    // вызовы не разбирает (см. §1.1 ТЗ: динамический слой C1).
    "common_module_calls_via_qualified_literal",
    // `ОбщийМодуль(Выражение)` (имя модуля не литерал) и
    // квалифицированная/метод-форма `ОбщегоНазначения.ОбщийМодуль("X")`
    // (литерал при не-платформенном приёмнике): по этим строкам обращение к
    // объекту `CommonModule` не извлекается — цель не становится видимой.
    "common_module_via_qualified_or_expression",
];

/// Виды `usage_kind`, которые кладёт индексер канала макетов. Если в
/// `metadata_code_usages` строки есть, а ни одной строки такого вида нет —
/// полнота канала макетов не подтверждена, и его нулевые счётчики нельзя
/// читать как «ссылок нет» (A4). Причина из данных НЕразличима: строк такого
/// вида нет и когда индекс собран прежним бинарником, и когда в конфигурации
/// нет ни одного вызова `ПолучитьМакет` (B2).
const TEMPLATE_CALL_KINDS: &[&str] = &[
    "template_call",
    "common_template_call",
    "template_call_via_manager",
    "template_call_via_receiver",
];

/// Виды `usage_kind`, отсутствие которых в НЕпустой `metadata_code_usages`
/// означает «полнота соответствующего канала не подтверждена» (Требование
/// 0/2.3): строки такого вида не появились (прежний индекс) ЛИБО обращений
/// такой формы в конфигурации нет — из данных неразличимо; в обоих случаях
/// нулевой счётчик канала нельзя читать как «ссылок нет». Пара — (вид строк,
/// имя формы в `channels_not_applied`).
const GLOBAL_KINDS: &[(&str, &str)] = &[
    ("common_picture", "common_picture_usages_not_indexed"),
    ("style_item", "style_item_usages_not_indexed"),
    // F1b: строковый доступ к общему модулю `ОбщийМодуль("Имя")`.
    (
        "common_module_via_literal",
        "common_module_literal_usages_not_indexed",
    ),
];

/// Сколько владельцев макетов класть в один `IN (…)` запроса канала строковых
/// путей: индексированный seek по `idx_mcu_ref` на каждый чанк.
const OWNER_CHUNK: usize = 400;

/// RAII-гаситель interrupt-таймера: `abort` при выходе из `execute` — в том
/// числе при отмене future (ранний `return` тоже проходит через `Drop`).
/// Без этого задача-таймер переживает отменённый вызов и дёргает
/// `sqlite3_interrupt` по соединению, уже отданному другому запросу (A10).
struct QueryTimer(tokio::task::JoinHandle<()>);

impl Drop for QueryTimer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct FindUnreferencedObjectsTool;

impl IndexTool for FindUnreferencedObjectsTool {
    fn name(&self) -> &str {
        "find_unreferenced_objects"
    }

    fn description(&self) -> &str {
        "Объекты метаданных 1С, на которые не ссылается ни один проиндексированный \
         канал ссылок, — кандидаты «мёртвых» объектов (макеты, подсистемы и пр.). \
         Каналы: data_links (структурные ссылки из метаданных), role_rights (права \
         ролей), metadata_code_usages (прямые обращения в коде: менеджер коллекции, \
         тип-ссылка, путь в запросе, общие картинки `БиблиотекаКартинок.X`/\
         `CommonPicture.X`, элементы стиля `ЦветаСтиля.X`/`style:X`), канал общих \
         модулей (вызовы `Модуль.Метод()` через proc_call_graph + модули-обработчики \
         подписок на события + строковый доступ `ОбщийМодуль(\"Имя\")`), канал макетов из \
         четырёх источников — строковые пути «Справочник.X.Макет» в текстах \
         запросов, ПолучитьМакет(\"X\") (владелец резолвится по модулю/пути), \
         ПолучитьОбщийМакет(\"X\"), метод-форма Справочники.X.ПолучитьМакет(\"Y\"). \
         Subsystem/Language ссылочно не адресуемы — исключены в not_addressable. \
         Это НЕ доказательство мёртвости: results — кандидаты на проверку; \
         channels_applied показывает, что именно проверено, template_call_sites_unresolved \
         — сколько МЕСТ вызова макетов не удалось привязать к владельцу, \
         template_call_sites_missing_target — сколько мест указывают на макет вне \
         реестра, common_module_literal_sites_missing_target — сколько мест \
         ОбщийМодуль(\"Имя\") указывают на общий модуль вне реестра, \
         channels_not_applied — какие формы не индексируются. При \
         устаревшем индексе (stale=true) числа не отдаются (results=null). Ответ: \
         results (страницами по бюджету) + results_total/shown/offset/has_more, \
         by_meta_type, channels_applied, channels_not_applied, formula_version. \
         For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Алиас репозитория" },
                "meta_type": {
                    "type": "string",
                    "description": "Фильтр по виду объекта: 'Template', 'CommonTemplate', 'Catalog'/'Справочник', 'Document' и т.п. (RU-формы нормализуются). Сужает и results, и счётчики."
                },
                "offset": {
                    "type": "integer",
                    "description": "Смещение страницы в полном наборе `results`. Без параметра — с начала.",
                    "minimum": 0
                },
                "max_response_bytes": {
                    "type": "integer",
                    "description": "Бюджет размера ЭТОГО ответа в байтах — перекрывает серверный на один вызов. Больше бюджет — больше строк в странице."
                }
            },
            "required": ["repo"]
        })
    }

    fn applicable_languages(&self) -> Option<&'static [&'static str]> {
        Some(&["bsl"])
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Value> + Send + 'a>> {
        Box::pin(async move {
            let raw_meta_type = args
                .get("meta_type")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            // RU-форма типа (`Справочник`) → каноническая (`Catalog`); неизвестная
            // остаётся как есть (SQL по несуществующему типу просто вернёт пусто).
            let meta_type = raw_meta_type.map(|t| {
                crate::code_usages::canonical_meta_type(t)
                    .unwrap_or(t)
                    .to_string()
            });
            let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let budget = cap::resolve_request_budget(
                args.get("max_response_bytes")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize),
            );

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("storage pool: {}", e)
                    }));
                }
            };

            // Требование 0: при устаревшем индексе числа не отдаются вовсе
            // (иначе смешение версий данных читается как содержательный ответ).
            let version = storage.data_version_status();
            if version.stale {
                return crate::tools::wrap_with_meta(
                    "find_unreferenced_objects",
                    json!({
                        "stale": true,
                        "results": null,
                        "results_total": null,
                        "data_version": {
                            "built": version.built,
                            "current": version.current,
                            "stale": version.stale,
                        },
                        "hint": format!(
                            "Индекс собран прежней версией данных (built={}, current={}): \
                             числа не отдаются, чтобы не смешивать несовместимые формулы. \
                             Переиндексируйте репозиторий и повторите вызов.",
                            version.built, version.current
                        ),
                    }),
                    Vec::new(),
                );
            }

            // interrupt-таймаут: тот же паттерн, что в find_references/bsl_sql,
            // но снятие — через Drop-стражу (A10): abort срабатывает и при
            // отмене future, а не только на штатных путях.
            let handle = storage.conn().get_interrupt_handle();
            let _timer = QueryTimer(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(QUERY_TIMEOUT_SECS)).await;
                handle.interrupt();
            }));

            // Синхронную работу с SQLite (rusqlite) уводим в блокирующий пул:
            // на async-воркере она занимает поток целиком, и задача-таймер не
            // успевает дёрнуть sqlite3_interrupt, пока запрос идёт. `storage`
            // (PooledStorage) держит соединение и `'static` — задача владеет им
            // до конца, соединение возвращается в пул по Drop.
            //
            // Отмена future по `tool_timeout` вызывающей стороны эту задачу НЕ
            // останавливает: `spawn_blocking` отмене не подлежит, обход
            // доводится до конца и соединение освобождается только по его
            // завершении (замер E-5: ≈0,7 с после отмены на живом индексе).
            // Результат отменённого вызова отбрасывается; повторно дёрнуть таймер
            // нельзя — он уже погашен `Drop`-стражей. Штатный путь выхода —
            // `sqlite3_interrupt` из `QueryTimer`: обход прекращается ошибкой.
            let mt = meta_type.clone();
            let scan =
                tokio::task::spawn_blocking(move || scan_repository(storage.conn(), mt.as_deref()))
                    .await;
            let mut scan = match scan {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("database error: {}", e)
                    }));
                }
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("scan task failed: {}", e)
                    }));
                }
            };

            // Результаты переносим в ответ; счётчики каналов остаются в `scan`
            // (partial move), поэтому страница и обёртка собираются отдельно.
            let results = std::mem::take(&mut scan.results);
            page_and_wrap(
                results,
                &scan,
                meta_type.as_deref(),
                ctx.repo,
                offset,
                budget.applied,
            )
        })
    }
}

/// Собрать обёрнутый ответ (`{result, _meta}`) со страницей `results` начиная с
/// `offset`, укладывающийся по размеру в `budget` байт (B1).
///
/// Конверт — всё, кроме наполнения массива `results` (шапка каналов,
/// `not_addressable`, `channels_not_applied`, `hint` и служебная обёртка), —
/// измеряется фактически: бюджет набора уменьшается на РЕАЛЬНОЕ превышение
/// конверта над резервом `PAGE_OVERHEAD_BYTES`, а не на сам резерв. Иначе
/// фиксированная шапка канала макетов (A2/A4) выносит ответ за запрошенный
/// бюджет. Превышение `page_by_bytes` над бюджетом (окно одной итерации)
/// ужимается повторным проходом, пока итоговый JSON не уложится в бюджет.
///
/// `budget == 0` — страж выключен, отдаётся весь остаток набора. Массив
/// `results` не обнуляется: при непустом наборе в странице всегда ≥1 элемент
/// (первый берётся даже сверх бюджета, иначе `offset` не сдвигался бы).
fn page_and_wrap(
    results: Vec<Value>,
    scan: &Scan,
    meta_type: Option<&str>,
    repo: &str,
    offset: usize,
    budget: usize,
) -> Value {
    let total = results.len();
    let base_hint = build_hint(scan, meta_type);
    // Всё, кроме `results`: клонируется на каждую итерацию подбора бюджета.
    let base = json!({
        "by_meta_type": scan.by_meta_type,
        "channels_applied": {
            "data_links": scan.channels[0],
            "role_rights": scan.channels[1],
            "code_usages": scan.channels[2],
            "template_paths": scan.channels[3],
            "template_calls": scan.channels[4],
            "common_template_calls": scan.channels[5],
            "template_calls_via_manager": scan.channels[6],
            "common_pictures": scan.channels[7],
            "style_items": scan.channels[8],
            "common_modules": scan.channels[9],
            "common_modules_via_subscription": scan.channels[10],
            "common_modules_via_literal": scan.channels[11],
        },
        "not_addressable": scan.not_addressable,
        "template_call_sites_unresolved": scan.template_call_sites_unresolved,
        "template_call_sites_missing_target": scan.template_call_sites_missing_target,
        "common_module_literal_sites_missing_target": scan
            .common_module_literal_sites_missing_target,
        "template_call_sites_uncovered": {
            "via_receiver": scan.template_call_sites_uncovered,
        },
        "channels_not_applied": scan.channels_not_applied,
        "formula_version": FORMULA_VERSION,
    });

    // Уменьшаемый только вниз бюджет набора; ≥1, чтобы не включить
    // «страж выключен» (budget == 0 отдал бы весь набор).
    let mut page_budget = budget;
    loop {
        let page = cap::page_by_bytes(results.clone(), offset, page_budget);
        let shown = page.shown;
        let page_offset = page.offset;
        let has_more = page.has_more;
        let mut hint = base_hint.clone();
        if has_more {
            let next = page_offset + shown;
            let mut call_args: Vec<(&str, Value)> =
                vec![("repo", json!(repo)), ("offset", json!(next))];
            if let Some(mt) = meta_type {
                call_args.push(("meta_type", json!(mt)));
            }
            hint.push_str(&format!(
                "\nПоказано {} из {}. Следующая страница — 1 вызов:\n{}",
                shown,
                total,
                cap::next_call_hint("find_unreferenced_objects", &call_args)
            ));
        }
        let mut out = base.clone();
        if let Some(obj) = out.as_object_mut() {
            obj.insert("results".to_string(), Value::Array(page.items));
            obj.insert("results_shown".to_string(), json!(shown));
            obj.insert("results_total".to_string(), json!(total));
            obj.insert("results_offset".to_string(), json!(page_offset));
            obj.insert("results_has_more".to_string(), json!(has_more));
            obj.insert("hint".to_string(), json!(hint));
        }
        let wrapped = crate::tools::wrap_with_meta("find_unreferenced_objects", out, Vec::new());
        if budget == 0 {
            return wrapped; // страж выключен — весь остаток набора
        }
        let size = serde_json::to_string(&wrapped)
            .map(|s| s.len())
            .unwrap_or(usize::MAX);
        if size <= budget || page_budget <= 1 {
            return wrapped;
        }
        // Ужать бюджет набора на фактическое превышение и повторить.
        page_budget = page_budget.saturating_sub(size - budget).max(1);
    }
}

/// Итог обхода: кандидаты без ссылок + счётчики применённых каналов.
struct Scan {
    /// Адресуемые кандидаты (`{full_name, meta_type, name}`), отсортированы по
    /// (meta_type, full_name).
    results: Vec<Value>,
    /// Разбивка `results` по meta_type (сумма = results.len()).
    by_meta_type: Map<String, Value>,
    /// Сколько объектов снял каждый канал:
    /// [data_links, role_rights, code_usages, template_paths, template_calls,
    /// common_template_calls, template_calls_via_manager, common_pictures,
    /// style_items, common_modules, common_modules_via_subscription,
    /// common_modules_via_literal]. Каналы пересекаются.
    channels: [usize; 12],
    /// Кандидаты ссылочно не адресуемых типов (Subsystem/Language) — не в results.
    not_addressable: Map<String, Value>,
    /// МЕСТА вызова (не объекты): `ПолучитьМакет`, которым не удалось
    /// определить владельца модуля.
    template_call_sites_unresolved: usize,
    /// МЕСТА вызова (не объекты): `Справочники.X.ПолучитьМакет("Y")`, где цель
    /// `<владелец>.Template.Y` отсутствует в реестре объектов («цель вне
    /// реестра»). Отличается от `template_call_sites_unresolved`: там владелец
    /// не выведен вовсе.
    template_call_sites_missing_target: usize,
    /// МЕСТА вызова (не объекты): `ОбщийМодуль("Имя")`
    /// (`usage_kind = common_module_via_literal`), где цель
    /// `CommonModule.<Имя>` отсутствует в реестре объектов («цель вне
    /// реестра») — аналог `template_call_sites_missing_target` для строкового
    /// канала общих модулей (A-асимметрия метрик).
    common_module_literal_sites_missing_target: usize,
    /// МЕСТА вызова (не объекты): непокрытая форма
    /// `template_call_via_receiver` — метод-вызов с нераспознанным приёмником
    /// (владельца не вывести) — счётчик.
    template_call_sites_uncovered: usize,
    /// Формы обращения к общим модулям/макетам, которые НЕ индексируются
    /// (+ `template_calls_not_indexed` / `*_usages_not_indexed` /
    /// `common_module_calls_not_indexed` на индексе без соответствующих строк).
    /// Требование 0 ТЗ.
    channels_not_applied: Vec<String>,
}

/// Одна строка перечня объектов с готовыми флагами простых каналов
/// (флаги считает SQL через EXISTS по индексированным ключам; виды обращений —
/// одним корреляционным подзапросом).
struct ObjectRow {
    full_name: String,
    full_name_key: String,
    meta_type: String,
    name: String,
    ch_data_links: bool,
    ch_role_rights: bool,
    ch_code_usages: bool,
    ch_common_picture: bool,
    ch_style_item: bool,
    /// Среди обращений объекта есть `common_module_via_literal`
    /// (`ОбщийМодуль("Имя")`) — счётчик канала общих модулей по строке.
    ch_common_module_literal: bool,
}

/// Полный проход: кандидаты без ссылок по всем каналам формулы "c2-v3".
fn scan_repository(conn: &rusqlite::Connection, meta_type: Option<&str>) -> rusqlite::Result<Scan> {
    // ОДИН проход по `metadata_code_usages` (E-11): из него выводятся и виды
    // обращений каждого объекта, и счётчики видов (замена серии COUNT/EXISTS и
    // двух проходов «цель вне реестра»). До этого на каждый вызов приходилось
    // ~6 полных сканов таблицы и коррелированный GROUP_CONCAT на каждый объект.
    let usage = load_usage_index(conn)?;
    // Ключи целей метрик «цель вне реестра»: макеты и общие модули реестра
    // (по одному seek по idx_metadata_objects_meta_type).
    let template_keys = load_meta_type_keys(conn, "Template")?;
    let common_module_keys = load_meta_type_keys(conn, "CommonModule")?;
    let objects = load_objects(conn, meta_type, &usage)?;

    // Владельцы макетов и имена общих макетов, до которых вообще может дойти
    // канал макетов: считаем по кандидатам без ссылок по простым каналам,
    // чтобы запросы канала были узкими (seek по индексу), а не полным сканом.
    let mut owners: Vec<String> = Vec::new();
    let mut owners_seen: HashSet<String> = HashSet::new();
    let mut common_members: HashSet<String> = HashSet::new();
    for obj in &objects {
        if obj.ch_data_links
            || obj.ch_role_rights
            || obj.ch_code_usages
            || obj.ch_common_picture
            || obj.ch_style_item
        {
            continue;
        }
        let key = &obj.full_name_key;
        if obj.meta_type == "Template" {
            if let Some((owner, _)) = key.split_once(".template.") {
                if owners_seen.insert(owner.to_string()) {
                    owners.push(owner.to_string());
                }
            }
        } else if let Some(member) = key.strip_prefix("commontemplate.") {
            common_members.insert(member.to_string());
        }
    }

    let template_paths = load_template_path_pairs(conn, &owners)?;
    let (template_calls, template_call_sites_unresolved) = load_template_call_pairs(conn)?;
    let manager_calls = load_manager_template_pairs(conn, &owners)?;
    let common_calls = load_common_template_calls(conn, &common_members)?;
    // Метрики «цель вне реестра» и непокрытая форма считаются из того же
    // прохода, что и виды обращений (см. `load_usage_index`).
    let template_call_sites_uncovered = usage.uncovered_template_calls;
    let template_call_sites_missing_target = usage.missing_template_calls(&template_keys)?;
    let common_module_literal_sites_missing_target =
        usage.missing_common_module_literals(&common_module_keys)?;
    // Канал общих модулей: объекты, чьи модули адресованы ВНЕШНИМИ рёбрами
    // графа (a) и/или назначены обработчиками подписок (c).
    let common_modules = load_common_module_index(conn)?;
    let common_module_hits = load_common_module_hits(conn, &common_modules)?;
    let subscription_hits = load_subscription_module_hits(conn, &common_modules)?;
    // Каналы с новыми видами данных: если строк такого вида в базе нет —
    // полнота канала не подтверждена (Требование 0/2.3).
    let mut channels_not_applied: Vec<String> = NOT_INDEXED_TEMPLATE_FORMS
        .iter()
        .map(|s| s.to_string())
        .collect();
    for form in NOT_INDEXED_COMMON_MODULE_FORMS {
        channels_not_applied.push((*form).to_string());
    }
    if !usage.template_call_kinds_present() {
        channels_not_applied.push("template_calls_not_indexed".to_string());
    }
    for (kind, flag) in GLOBAL_KINDS {
        if !usage.kind_present(kind) {
            channels_not_applied.push((*flag).to_string());
        }
    }
    if !common_module_channel_present(conn)? {
        channels_not_applied.push("common_module_calls_not_indexed".to_string());
    }

    let mut channels = [0usize; 12];
    let mut results: Vec<Value> = Vec::new();
    let mut by_meta_type = Map::new();
    let mut not_addressable = Map::new();
    for obj in &objects {
        let key = &obj.full_name_key;
        let mut hit = false;
        if obj.ch_data_links {
            channels[0] += 1;
            hit = true;
        }
        if obj.ch_role_rights {
            channels[1] += 1;
            hit = true;
        }
        if obj.ch_code_usages {
            channels[2] += 1;
            hit = true;
        }
        if obj.ch_common_picture {
            channels[7] += 1;
            hit = true;
        }
        if obj.ch_style_item {
            channels[8] += 1;
            hit = true;
        }
        if common_module_hits.contains(key) {
            channels[9] += 1;
            hit = true;
        }
        if subscription_hits.contains(key) {
            channels[10] += 1;
            hit = true;
        }
        if obj.ch_common_module_literal {
            channels[11] += 1;
            hit = true;
        }
        if obj.meta_type == "Template" {
            // Имя макета к регистру не чувствительно (ключи в базе считает Rust
            // для кириллицы; здесь пара собирается по ключу владельца + lower(имени)).
            if let Some((owner, _)) = key.split_once(".template.") {
                let pair = (owner.to_string(), obj.name.to_lowercase());
                if template_paths.contains(&pair) {
                    channels[3] += 1;
                    hit = true;
                }
                if template_calls.contains(&pair) {
                    channels[4] += 1;
                    hit = true;
                }
                if manager_calls.contains(&pair) {
                    channels[6] += 1;
                    hit = true;
                }
            }
        } else if let Some(member) = key.strip_prefix("commontemplate.") {
            if common_calls.contains(member) {
                channels[5] += 1;
                hit = true;
            }
        }
        if hit {
            continue;
        }
        if NOT_ADDRESSABLE.contains(&obj.meta_type.as_str()) {
            bump(&mut not_addressable, &obj.meta_type);
            continue;
        }
        bump(&mut by_meta_type, &obj.meta_type);
        results.push(json!({
            "full_name": obj.full_name,
            "meta_type": obj.meta_type,
            "name": obj.name,
        }));
    }

    Ok(Scan {
        results,
        by_meta_type,
        channels,
        not_addressable,
        template_call_sites_unresolved,
        template_call_sites_missing_target,
        common_module_literal_sites_missing_target,
        template_call_sites_uncovered,
        channels_not_applied,
    })
}

/// Биты видов обращений объекта в коде (см. [`UsageIndex::flags`]).
const KIND_ANY: u8 = 1;
const KIND_COMMON_PICTURE: u8 = 2;
const KIND_STYLE_ITEM: u8 = 4;
const KIND_COMMON_MODULE_LITERAL: u8 = 8;

/// ОДИН проход по `metadata_code_usages`, из которого выводится всё, что нужно
/// обходу от этой таблицы (E-11):
///
///   * `flags` — виды обращений каждого объекта (биты [`KIND_ANY`],
///     [`KIND_COMMON_PICTURE`], [`KIND_STYLE_ITEM`],
///     [`KIND_COMMON_MODULE_LITERAL`]) — вместо коррелированного
///     `GROUP_CONCAT(DISTINCT usage_kind)` на каждый объект (TEMPORARY B-TREE на
///     строку и seek по `idx_mcu_ref` на каждый объект);
///   * `kind_counts` — счётчики строк по видам: из них выводятся и наличие вида
///     (`channels_not_applied`), и непокрытая форма `template_call_via_receiver`
///     — вместо серии `COUNT(*)`/`COUNT(*) … usage_kind = ?` (индекса по
///     `usage_kind` нет, каждый такой COUNT — полный скан таблицы);
///   * `manager_template_calls` / `common_module_literal_keys` — МЕСТА вызовов,
///     нужные метрикам «цель вне реестра» — вместо ещё двух полных сканов.
///
/// Замер на prod3 (31 551 строка): один проход 26 мс против ~70 мс у серии
/// запросов, которые он заменяет (не считая случайных чтений по индексам).
struct UsageIndex {
    /// `object_ref_key` → битовая маска видов ([`KIND_ANY`] и т.д.).
    flags: HashMap<String, u8>,
    /// `usage_kind` → число строк в таблице.
    kind_counts: HashMap<String, i64>,
    /// Число строк непокрытой формы `template_call_via_receiver`.
    uncovered_template_calls: usize,
    /// `(ключ владельца, имя макета как в литерале)` из строк
    /// `template_call_via_manager` — по ним считается «цель вне реестра».
    manager_template_calls: Vec<(String, String)>,
    /// `object_ref_key` строк `common_module_via_literal` — по ним считается
    /// «общий модуль вне реестра».
    common_module_literal_keys: Vec<String>,
}

impl UsageIndex {
    /// Есть ли строки вида `kind`? Пустая таблица — «судить не о чем» (true):
    /// свежая база без кода не должна давать оговорку о полноте канала.
    fn kind_present(&self, kind: &str) -> bool {
        if self.kind_counts.is_empty() {
            return true;
        }
        self.kind_counts.get(kind).is_some_and(|n| *n > 0)
    }

    /// Материализован ли канал макетов: в таблице есть строки, и среди них есть
    /// хотя бы одна строка вида из [`TEMPLATE_CALL_KINDS`].
    fn template_call_kinds_present(&self) -> bool {
        if self.kind_counts.is_empty() {
            return true;
        }
        TEMPLATE_CALL_KINDS.iter().any(|k| self.kind_present(k))
    }

    /// МЕСТА вызова `Справочники.X.ПолучитьМакет("Y")`, где цель
    /// `<владелец>.Template.Y` отсутствует в реестре объектов. Сравнение ключей
    /// ведём в Rust: `lower()` в SQLite кириллицу не берёт.
    fn missing_template_calls(&self, templates: &HashSet<String>) -> rusqlite::Result<usize> {
        Ok(self
            .manager_template_calls
            .iter()
            .filter(|(owner_key, member)| {
                !templates.contains(&format!("{}.template.{}", owner_key, member.to_lowercase()))
            })
            .count())
    }

    /// МЕСТА вызова `ОбщийМодуль("Имя")`, где цель `CommonModule.<Имя>`
    /// отсутствует в реестре объектов.
    fn missing_common_module_literals(&self, modules: &HashSet<String>) -> rusqlite::Result<usize> {
        Ok(self
            .common_module_literal_keys
            .iter()
            .filter(|key| !key.is_empty() && !modules.contains(*key))
            .count())
    }
}

/// Собрать [`UsageIndex`] одним проходом по `metadata_code_usages`.
fn load_usage_index(conn: &rusqlite::Connection) -> rusqlite::Result<UsageIndex> {
    let mut index = UsageIndex {
        flags: HashMap::new(),
        kind_counts: HashMap::new(),
        uncovered_template_calls: 0,
        manager_template_calls: Vec::new(),
        common_module_literal_keys: Vec::new(),
    };
    let mut stmt = conn.prepare(
        "SELECT object_ref_key, usage_kind, member_path FROM metadata_code_usages \
         WHERE repo = ?",
    )?;
    let rows = stmt.query_map(params!["default"], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in rows {
        let (key, kind, member) = row?;
        *index.kind_counts.entry(kind.clone()).or_insert(0) += 1;
        let flags = index.flags.entry(key.clone()).or_insert(0);
        *flags |= KIND_ANY;
        match kind.as_str() {
            "common_picture" => *flags |= KIND_COMMON_PICTURE,
            "style_item" => *flags |= KIND_STYLE_ITEM,
            "common_module_via_literal" => {
                *flags |= KIND_COMMON_MODULE_LITERAL;
                index.common_module_literal_keys.push(key.clone());
            }
            "template_call_via_receiver" => index.uncovered_template_calls += 1,
            "template_call_via_manager" => {
                if let Some(member) = member.filter(|m| !m.is_empty()) {
                    index.manager_template_calls.push((key.clone(), member));
                }
            }
            _ => {}
        }
    }
    Ok(index)
}

/// Ключи (`full_name_key`) всех объектов указанного `meta_type` — цели метрик
/// «цель вне реестра». Один seek по `idx_metadata_objects_meta_type`.
fn load_meta_type_keys(
    conn: &rusqlite::Connection,
    meta_type: &str,
) -> rusqlite::Result<HashSet<String>> {
    let mut out = HashSet::new();
    let mut stmt = conn
        .prepare("SELECT full_name_key FROM metadata_objects WHERE repo = ? AND meta_type = ?")?;
    let rows = stmt.query_map(params!["default", meta_type], |r| r.get::<_, String>(0))?;
    for row in rows {
        out.insert(row?);
    }
    Ok(out)
}

/// Перечень объектов репо (с фильтром по типу) + флаги простых каналов.
/// Ключи ссылок берутся тремя последовательными проходами по covering-индексам
/// (`idx_dl_to_key`, `idx_rr_object_key`) и одним проходом [`UsageIndex`] —
/// вместо коррелированных EXISTS/`GROUP_CONCAT` на каждый объект (E-11:
/// seek на объект давал ~4 тыс. случайных чтений и TEMPORARY B-TREE на строку).
///
/// Фильтр `meta_type` подставляется в SQL ТОЛЬКО когда задан: `COLLATE NOCASE`
/// (тип уже канонизирован на входе `execute`) ломал seek по
/// `idx_metadata_objects_meta_type` — план опускался до `repo=?` (A7), а
/// выражение `?2 IS NULL OR …` не давало планировщику использовать колонку
/// `meta_type` даже без COLLATE.
fn load_objects(
    conn: &rusqlite::Connection,
    meta_type: Option<&str>,
    usage: &UsageIndex,
) -> rusqlite::Result<Vec<ObjectRow>> {
    // Множества ключей-целей: один проход по индексу вместо EXISTS на объект.
    let mut referenced: HashSet<String> = HashSet::new();
    {
        let mut stmt =
            conn.prepare("SELECT DISTINCT to_object_key FROM data_links WHERE repo = ?")?;
        let rows = stmt.query_map(params!["default"], |r| r.get::<_, String>(0))?;
        for row in rows {
            referenced.insert(row?);
        }
    }
    let mut rights_granted: HashSet<String> = HashSet::new();
    {
        let mut stmt =
            conn.prepare("SELECT DISTINCT object_name_key FROM role_rights WHERE repo = ?")?;
        let rows = stmt.query_map(params!["default"], |r| r.get::<_, String>(0))?;
        for row in rows {
            rights_granted.insert(row?);
        }
    }

    const COLS: &str = "\
        SELECT mo.full_name, mo.full_name_key, mo.meta_type, mo.name \
        FROM metadata_objects mo \
        WHERE mo.repo = ?1";
    let map_row = |r: &rusqlite::Row| {
        let full_name_key: String = r.get(1)?;
        // Виды обращений объекта — из одного прохода `load_usage_index`
        // (E10/E-11): универсальный канал `code_usages` и подсчёт по видам
        // `common_picture`/`style_item` выводятся из одного и того же набора.
        let kinds = usage.flags.get(&full_name_key).copied().unwrap_or(0);
        Ok(ObjectRow {
            ch_data_links: referenced.contains(&full_name_key),
            ch_role_rights: rights_granted.contains(&full_name_key),
            full_name: r.get(0)?,
            full_name_key,
            meta_type: r.get(2)?,
            name: r.get(3)?,
            ch_code_usages: kinds & KIND_ANY != 0,
            ch_common_picture: kinds & KIND_COMMON_PICTURE != 0,
            ch_style_item: kinds & KIND_STYLE_ITEM != 0,
            // F1b: строковый доступ `ОбщийМодуль("Имя")` — тот же проход, что
            // универсальный `code_usages`, без отдельного EXISTS (E10).
            ch_common_module_literal: kinds & KIND_COMMON_MODULE_LITERAL != 0,
        })
    };
    match meta_type {
        Some(mt) => {
            let sql = format!("{COLS} AND mo.meta_type = ?2 ORDER BY mo.meta_type, mo.full_name");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params!["default", mt], map_row)?;
            rows.collect()
        }
        None => {
            let sql = format!("{COLS} ORDER BY mo.meta_type, mo.full_name");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params!["default"], map_row)?;
            rows.collect()
        }
    }
}

/// Пары (ключ владельца, lower(имя макета)) из строковых путей метаданных:
/// `query`-строки с непустым `member_path`. Запрос идёт чанками владельцев —
/// только по индексному seek, без полного скана `metadata_code_usages`.
fn load_template_path_pairs(
    conn: &rusqlite::Connection,
    owners: &[String],
) -> rusqlite::Result<HashSet<(String, String)>> {
    let mut out = HashSet::new();
    for chunk in owners.chunks(OWNER_CHUNK) {
        let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT DISTINCT object_ref_key, member_path FROM metadata_code_usages \
             WHERE repo = ? AND usage_kind = 'query' AND member_path IS NOT NULL \
               AND object_ref_key IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut args: Vec<&str> = Vec::with_capacity(chunk.len() + 1);
        args.push("default");
        args.extend(chunk.iter().map(String::as_str));
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (owner, member) = row?;
            if let Some(m) = member.filter(|m| !m.is_empty()) {
                out.insert((owner, m.to_lowercase()));
            }
        }
    }
    Ok(out)
}

/// Пары (ключ владельца, lower(имя макета)) из `ПолучитьМакет("X")`. Владелец
/// резолвится по `file_path` вызывающего модуля: сначала через
/// `metadata_modules.code_path → object_name`, затем разбором пути. Возвращает
/// пары и число вызовов, у которых владелец не определился.
fn load_template_call_pairs(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<(HashSet<(String, String)>, usize)> {
    // code_path → object_name; при (неожидаемых) дублях пути берём первую
    // строку по (code_path, extension_name) — порядок детерминирован.
    let mut modules: HashMap<String, String> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT code_path, object_name FROM metadata_modules \
             WHERE repo = ? AND code_path IS NOT NULL AND code_path <> '' \
             ORDER BY code_path, extension_name",
        )?;
        let rows = stmt.query_map(params!["default"], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (path, object_name) = row?;
            modules.entry(path).or_insert(object_name);
        }
    }
    // Строки нового вида ищутся по ключу `template` — это seek по idx_mcu_ref,
    // а не перебор таблицы (строк вида мало: сотни на конфигурацию).
    let mut out = HashSet::new();
    let mut unresolved = 0usize;
    let mut stmt = conn.prepare(
        "SELECT file_path, member_path FROM metadata_code_usages \
         WHERE repo = ? AND object_ref_key = 'template' AND usage_kind = 'template_call' \
           AND member_path IS NOT NULL",
    )?;
    let rows = stmt.query_map(params!["default"], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (file_path, member) = row?;
        if member.is_empty() {
            continue;
        }
        let owner = modules
            .get(&file_path)
            .and_then(|object_name| owner_key_from_module_object_name(object_name))
            .or_else(|| owner_key_from_path(&file_path));
        match owner {
            Some(owner) => {
                out.insert((owner, member.to_lowercase()));
            }
            None => unresolved += 1,
        }
    }
    Ok((out, unresolved))
}

/// lower(имена общих макетов) из `ПолучитьОбщийМакет("X")`. `common_members`
/// сужает выборку до кандидатов; при пустом наборе запрос не выполняется.
fn load_common_template_calls(
    conn: &rusqlite::Connection,
    common_members: &HashSet<String>,
) -> rusqlite::Result<HashSet<String>> {
    let mut out = HashSet::new();
    if common_members.is_empty() {
        return Ok(out);
    }
    let mut stmt = conn.prepare(
        "SELECT DISTINCT member_path FROM metadata_code_usages \
         WHERE repo = ? AND object_ref_key = 'commontemplate' \
           AND usage_kind = 'common_template_call' AND member_path IS NOT NULL",
    )?;
    let rows = stmt.query_map(params!["default"], |r| r.get::<_, String>(0))?;
    for row in rows {
        let member = row?;
        if !member.is_empty() {
            out.insert(member.to_lowercase());
        }
    }
    Ok(out)
}

/// Пары (ключ владельца, lower(имя макета)) из метод-формы
/// `Справочники.X.ПолучитьМакет("Y")` (`usage_kind = 'template_call_via_manager'`).
/// Владелец уже лежит в `object_ref_key` (`catalog.x`), резолв по файлу не
/// нужен. `owners` сужает выборку чанками — seek по `idx_mcu_ref`.
fn load_manager_template_pairs(
    conn: &rusqlite::Connection,
    owners: &[String],
) -> rusqlite::Result<HashSet<(String, String)>> {
    let mut out = HashSet::new();
    for chunk in owners.chunks(OWNER_CHUNK) {
        let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT DISTINCT object_ref_key, member_path FROM metadata_code_usages \
             WHERE repo = ? AND usage_kind = 'template_call_via_manager' \
               AND member_path IS NOT NULL AND object_ref_key IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut args: Vec<&str> = Vec::with_capacity(chunk.len() + 1);
        args.push("default");
        args.extend(chunk.iter().map(String::as_str));
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (owner, member) = row?;
            if let Some(m) = member.filter(|m| !m.is_empty()) {
                out.insert((owner, m.to_lowercase()));
            }
        }
    }
    Ok(out)
}

/// Общий шаг канала общих модулей: какие ОБЩИЕ модули вообще есть в индексе и
/// каким владельцам они соответствуют. Нужен и графу, и подпискам — оба
/// источника резолвят цель в один и тот же `owner_key` (`commonmodule.x`).
///
/// `object_name` в `metadata_modules` — папка выгрузки (plural):
/// `CommonModules.X`.
struct CommonModuleIndex {
    /// owner_key (`commonmodule.x`) → пути модулей этого объекта (у объекта их
    /// может быть несколько; у модуля расширения путь с префиксом области).
    paths_by_owner: HashMap<String, Vec<String>>,
    /// путь модуля → владельцы. E-4: один `code_path` теоретически могут делить
    /// несколько `metadata_modules` (разные владельцы) — храним ВСЕХ, иначе
    /// второй владелец терялся бы и всплывал ложным «мёртвым».
    owners_by_path: HashMap<String, Vec<String>>,
}

/// Построить [`CommonModuleIndex`] по `metadata_modules`.
fn load_common_module_index(conn: &rusqlite::Connection) -> rusqlite::Result<CommonModuleIndex> {
    let mut paths_by_owner: HashMap<String, Vec<String>> = HashMap::new();
    let mut owners_by_path: HashMap<String, Vec<String>> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT object_name, code_path FROM metadata_modules \
         WHERE repo = ? AND object_name LIKE 'CommonModules.%' \
           AND code_path IS NOT NULL AND code_path <> '' \
         ORDER BY object_name, extension_name",
    )?;
    let rows = stmt.query_map(params!["default"], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (object_name, code_path) = row?;
        let rest = &object_name["CommonModules.".len()..];
        let owner_key = format!("commonmodule.{}", rest.to_lowercase());
        let owners = owners_by_path.entry(code_path.clone()).or_default();
        if !owners.contains(&owner_key) {
            owners.push(owner_key.clone());
        }
        let paths = paths_by_owner.entry(owner_key).or_default();
        if !paths.contains(&code_path) {
            paths.push(code_path);
        }
    }
    Ok(CommonModuleIndex {
        paths_by_owner,
        owners_by_path,
    })
}

/// Объекты `CommonModule`, использованные вызовом `Модуль.Метод()` ИЗВНЕ:
/// резолвер графа вызовов уже проставил `callee_proc_key = <путь модуля>::<метод>`
/// (источник адреса — `metadata_modules.code_path`), поэтому владелец вызова
/// известен БЕЗ переиндексации и без чтения кода.
///
/// E1: раньше пути шли чанками по 400 OR-диапазонов `[p::, p:;)` — на ЗУП это
/// деградировало в скан (SQLite держит MULTI-INDEX OR лишь до ~5 термов:
/// n=2/5 → 0 мс, n=50 → 739 мс, n=400 → 6 060 мс; ≈20,5 с на вызов). Здесь ОДИН
/// проход по всем адресованным рёбрам (линейный по числу рёбер), а сопоставление
/// с путями идёт в Rust по префиксу `<path>::` (`rfind("::")`).
///
/// E2: self-рёбра не считаются использованием — ребро засчитывается, только если
/// путь вызывающего не совпадает НИ С ОДНИМ путём модулей этого объекта (у
/// объекта может быть несколько модулей).
fn load_common_module_hits(
    conn: &rusqlite::Connection,
    index: &CommonModuleIndex,
) -> rusqlite::Result<HashSet<String>> {
    if index.paths_by_owner.is_empty() {
        return Ok(HashSet::new());
    }

    // ОДИН проход по графу: какие объекты адресованы ВНЕШНИМИ рёбрами.
    //
    // Таблица читается ЦЕЛИКОМ (SCAN), а не поиском по частичному индексу
    // `idx_pcg_callee_key`: индекс не покрывает `caller_proc_key`, поэтому поиск
    // по нему тянет каждую строку из таблицы случайно (замер prod3: 205 мс
    // против 125 мс у последовательного прохода). Строки без адреса цели
    // (`callee_proc_key IS NULL`) отсеиваются в Rust.
    let mut hits: HashSet<String> = HashSet::new();
    let mut stmt = conn
        .prepare("SELECT caller_proc_key, callee_proc_key FROM proc_call_graph WHERE repo = ?")?;
    let rows = stmt.query_map(params!["default"], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    for row in rows {
        let (caller_key, callee_key) = row?;
        let Some(callee_key) = callee_key else {
            continue;
        };
        let Some(callee_path) = proc_key_path(&callee_key) else {
            continue;
        };
        let Some(owners) = index.owners_by_path.get(callee_path) else {
            continue;
        };
        // E-4: путь может принадлежать нескольким владельцам — проверяем каждого.
        for owner in owners {
            if hits.contains(owner) {
                continue;
            }
            // Внешним считаем ребро, где путь вызывающего не совпадает ни с одним
            // путём модулей объекта (self-вызов модуля использованием не является).
            let self_edge = match (proc_key_path(&caller_key), index.paths_by_owner.get(owner)) {
                (Some(caller_path), Some(owner_paths)) => {
                    owner_paths.iter().any(|p| p == caller_path)
                }
                _ => false,
            };
            if !self_edge {
                hits.insert(owner.clone());
            }
        }
    }
    Ok(hits)
}

/// Объекты `CommonModule`, назначенные обработчиками подписок на события
/// (`event_subscriptions.handler_module = CommonModule.X`). Платформа вызывает
/// обработчик САМА, а ребро подписки в `proc_call_graph` резолвится в ИМЯ
/// процедуры (`callee_proc_key IS NULL`, в нём лежит `Модуль.Метод`), поэтому
/// канал графа обработчик не видит вовсе: без этого источника модуль мог
/// попасть в `results`, хотя используется.
///
/// Резолв тот же, что у графа (F1a): имя модуля из подписки →
/// `metadata_modules` (`object_name = 'CommonModules.<X>'` с непустым
/// `code_path`, т.е. модуль есть в [`CommonModuleIndex`]) → `owner_key`. Короткая
/// форма имени (без префикса `CommonModule.`) тоже принимается — в
/// `event_subscriptions` пишется то, что стоит в XML.
fn load_subscription_module_hits(
    conn: &rusqlite::Connection,
    index: &CommonModuleIndex,
) -> rusqlite::Result<HashSet<String>> {
    if index.paths_by_owner.is_empty() {
        return Ok(HashSet::new());
    }
    let mut hits: HashSet<String> = HashSet::new();
    let mut stmt = conn.prepare(
        "SELECT DISTINCT handler_module FROM event_subscriptions \
         WHERE repo = ? AND handler_module <> ''",
    )?;
    let rows = stmt.query_map(params!["default"], |r| r.get::<_, String>(0))?;
    for row in rows {
        let module = row?;
        let name = strip_common_module_prefix(&module);
        if name.is_empty() {
            continue;
        }
        let owner = format!("commonmodule.{}", name.to_lowercase());
        if index.paths_by_owner.contains_key(&owner) {
            hits.insert(owner);
        }
    }
    Ok(hits)
}

/// Имя общего модуля из `handler_module` подписки: `CommonModule.X` → `X`;
/// короткая форма (`X`) отдаётся как есть. Префикс срезается по ASCII-регистру
/// (`commonmodule.`/`CommonModule.`), кириллица в имени не трогается; срез идёт
/// через `str::get` — байтовая граница проверяется, паники на многобайтной
/// кириллице в начале строки не будет.
fn strip_common_module_prefix(handler_module: &str) -> &str {
    const PREFIX: &str = "commonmodule.";
    match handler_module.get(PREFIX.len()..) {
        Some(rest) if handler_module[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) => rest,
        _ => handler_module,
    }
}

/// Путь модуля из адреса вызова `<путь>::<метод>`: всё до ПОСЛЕДНЕГО `::`
/// ([`str::rfind`], а не `split_once` — путь модуля сам может содержать `::`,
/// а имя метода — нет; E6). `None`, если разделителя нет.
fn proc_key_path(key: &str) -> Option<&str> {
    key.rfind("::").map(|i| &key[..i])
}

/// Материализован ли канал общих модулей: канал подтверждён ТОЛЬКО при
/// `graph_rows > 0 && keyed > 0 && modules > 0` (в `proc_call_graph` есть хотя бы
/// одно ребро с адресом цели `callee_proc_key`, а в `metadata_modules` — хотя бы
/// один общий модуль с непустым `code_path`). Полностью пустые таблицы — «судить
/// не о чем» (true). Иначе канал перечисляется в `channels_not_applied`
/// (`common_module_calls_not_indexed`): полнота не подтверждена (причина из
/// данных неразличима — прежний индекс ЛИБО конфигурация без общих модулей).
fn common_module_channel_present(conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
    let graph_rows: i64 = conn.query_row(
        "SELECT COUNT(*) FROM proc_call_graph WHERE repo = ?",
        params!["default"],
        |r| r.get(0),
    )?;
    let modules: i64 = conn.query_row(
        "SELECT COUNT(*) FROM metadata_modules \
         WHERE repo = ? AND object_name LIKE 'CommonModules.%' \
           AND code_path IS NOT NULL AND code_path <> ''",
        params!["default"],
        |r| r.get(0),
    )?;
    // Полностью пустой слой — «судить не о чем»: свежая база без кода не должна
    // давать ложную оговорку.
    if graph_rows == 0 && modules == 0 {
        return Ok(true);
    }
    // E5: канал подтверждён ТОЛЬКО при graph_rows > 0 && keyed > 0 && modules > 0.
    // Пустой граф при непустых модулях раньше проходил как «подтверждён»
    // (`modules > 0`), хотя адресов вызовов нет вовсе — нулевой счётчик канала
    // нельзя читать как «общих модулей не вызывают».
    if graph_rows == 0 {
        return Ok(false);
    }
    let keyed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM proc_call_graph WHERE repo = ? AND callee_proc_key IS NOT NULL",
        params!["default"],
        |r| r.get(0),
    )?;
    Ok(keyed > 0 && modules > 0)
}

/// Папка выгрузки (plural, как в путях и `metadata_modules.object_name`) →
/// канонический singular meta_type: `Catalogs` → `Catalog`. Карта — та же, что
/// у разбора путей метаданных ([`crate::index_extras::ALL_OBJECT_FOLDERS`]);
/// сверх неё принимаются уже-канонические типы (`Catalog`, `Документ`).
fn folder_to_meta_type(folder: &str) -> Option<&'static str> {
    crate::index_extras::ALL_OBJECT_FOLDERS
        .iter()
        .find(|(f, _)| *f == folder)
        .map(|(_, meta_type)| *meta_type)
        .or_else(|| crate::code_usages::canonical_meta_type(folder))
}

/// Ключ владельца макета по `metadata_modules.object_name`:
/// `Catalogs.Вакансии` → `catalog.вакансии`; у модулей форм и команд
/// (`Catalogs.X.Form.Ф`, `Catalogs.X.Command.К`) владелец макета — сам объект,
/// поэтому суффикс отрезается. `None` — папка/раскладка не опознаны.
fn owner_key_from_module_object_name(object_name: &str) -> Option<String> {
    let cut = [".Form.", ".Command."]
        .iter()
        .filter_map(|s| object_name.find(s))
        .min()
        .unwrap_or(object_name.len());
    let (folder, name) = object_name[..cut].split_once('.')?;
    let meta_type = folder_to_meta_type(folder)?;
    if name.is_empty() {
        return None;
    }
    Some(format!("{}.{}", meta_type, name).to_lowercase())
}

/// Фолбэк-резолв владельца по пути модуля: первая пара
/// `<ПапкаВыгрузки>/<Имя>` из `ALL_OBJECT_FOLDERS`. Тот же разбор, что у
/// индексеров путей метаданных, — префиксы (`base/`, `extensions/<EF>/`, `src/`)
/// не мешают. `None` — раскладка не похожа на выгрузку 1С.
fn owner_key_from_path(file_path: &str) -> Option<String> {
    let segments: Vec<&str> = file_path.split('/').collect();
    for pair in segments.windows(2) {
        if let Some(meta_type) = folder_to_meta_type(pair[0]) {
            if !pair[1].is_empty() {
                return Some(format!("{}.{}", meta_type, pair[1]).to_lowercase());
            }
        }
    }
    None
}

/// Счётчик в JSON-мапе: +1 к значению по ключу.
fn bump(map: &mut Map<String, Value>, key: &str) {
    let entry = map.entry(key.to_string()).or_insert_with(|| json!(0));
    if let Some(n) = entry.as_u64() {
        *entry = json!(n + 1);
    }
}

/// Подсказка «как читать ответ»: чем именно проверено, где оговорки.
fn build_hint(scan: &Scan, meta_type: Option<&str>) -> String {
    let mut hint = String::from(
        "Это НЕ доказательство мёртвости: results — объекты, к которым не нашлось \
         ни одной ссылки ни в одном из проверенных каналов. Проверено: data_links \
         (структурные ссылки из метаданных); role_rights (права ролей); code_usages \
         (обращения в коде: менеджер коллекции, тип-ссылка, путь в запросе); \
         common_pictures (БиблиотекаКартинок.X/CommonPicture.X в коде и XML форм); \
         style_items (ЦветаСтиля.X/ШрифтыСтиля.X в коде и style:X в XML); \
         common_modules (вызовы Модуль.Метод() — по рёбрам proc_call_graph); \
         common_modules_via_subscription (модули-обработчики подписок на события — \
         платформа зовёт их сама, ребра вызова у подписки нет); \
         common_modules_via_literal (строковый доступ ОбщийМодуль(\"Имя\")); \
         template_paths (строковые пути «Класс.Объект.Макет» в текстах запросов); \
         template_calls (ПолучитьМакет — владелец по metadata_modules/пути модуля); \
         common_template_calls (ПолучитьОбщийМакет); template_calls_via_manager \
         (Справочники.X.ПолучитьМакет — владелец из приёмника). channels_applied — \
         сколько объектов снял каждый канал (каналы пересекаются). Subsystem/Language \
         ссылочно не адресуемы — в not_addressable, не в results. Числа сравнимы \
         только при одной formula_version.",
    );
    if let Some(mt) = meta_type {
        hint.push_str(&format!(
            " Фильтр meta_type='{mt}' сужает и results, и счётчики каналов."
        ));
    }
    // Явная формулировка «форма не индексируется» для каждой позиции
    // channels_not_applied (Требование 0/2): даже при отсутствии счётчика
    // видно, какие формы не покрыты.
    hint.push_str(&format!(
        " Не индексируются как ссылки формы: {} \
         (см. channels_not_applied; у template_call_via_receiver число — \
         в template_call_sites_uncovered.via_receiver).",
        scan.channels_not_applied.join(", ")
    ));
    if scan
        .channels_not_applied
        .iter()
        .any(|c| c == "template_calls_not_indexed")
    {
        // B2: строк вида `template_call*` в базе нет. Причина из данных
        // НЕразличима (индекс собран прежним бинарником ЛИБО в конфигурации нет
        // вызовов `ПолучитьМакет`) — формулировка нейтральная, без утверждения
        // причины. Полнота канала макетов не подтверждена, независимо от того,
        // что остальные счётчики нулевые.
        hint.push_str(
            " ВНИМАНИЕ: строк вида «template_call*» в базе нет — полнота канала \
             макетов не подтверждена. Причина неразличима: индекс мог быть собран \
             прежним бинарником, либо в конфигурации нет ни одного вызова \
             ПолучитьМакет. Пока это так, нулевые счётчики каналов макетов и \
             «мёртвые» макеты в results нельзя читать как отсутствие ссылок: \
             переиндексируйте репозиторий новым бинарником. \
             channels_not_applied содержит template_calls_not_indexed.",
        );
    } else if scan.channels[3..=6].iter().all(|c| *c == 0) {
        hint.push_str(
            " Нулевые счётчики каналов макетов возможны и на старом индексе: строки \
             «template_call»/«common_template_call» появляются после переиндексации \
             новым бинарником.",
        );
    }
    // Полнота новых каналов (картинки/стили/общие модули) не подтверждена на
    // индексе без их данных: формулировка нейтральная, без утверждения причины.
    let unconfirmed: Vec<&str> = scan
        .channels_not_applied
        .iter()
        .map(String::as_str)
        .filter(|c| {
            matches!(
                *c,
                "common_picture_usages_not_indexed"
                    | "style_item_usages_not_indexed"
                    | "common_module_calls_not_indexed"
                    | "common_module_literal_usages_not_indexed"
            )
        })
        .collect();
    if !unconfirmed.is_empty() {
        hint.push_str(&format!(
            " ВНИМАНИЕ: полнота каналов {} не подтверждена (строк таких данных в базе \
             нет — прежний индекс либо отсутствие таких обращений в конфигурации; \
             из данных неразличимо). Нулевые счётчики этих каналов нельзя читать как \
             отсутствие ссылок: переиндексируйте репозиторий новым бинарником.",
            unconfirmed.join(", ")
        ));
    }
    if scan.template_call_sites_uncovered > 0 {
        hint.push_str(&format!(
            " {} мест вызова ПолучитьМакет с нераспознанным приёмником \
             (template_call_via_receiver) не индексируются как ссылки — владельца \
             макета по ним не вывести; среди results могут быть используемые макеты.",
            scan.template_call_sites_uncovered
        ));
    }
    if scan.template_call_sites_unresolved > 0 {
        hint.push_str(&format!(
            " ВНИМАНИЕ: {} мест вызова ПолучитьМакет не привязано к владельцу (module/путь \
             не опознаны) — среди результатов могут быть используемые макеты.",
            scan.template_call_sites_unresolved
        ));
    }
    if scan.template_call_sites_missing_target > 0 {
        hint.push_str(&format!(
            " {} мест вызова Справочники.X.ПолучитьМакет(\"Y\") указывают на макет, \
             которого нет в реестре объектов (template_call_sites_missing_target): \
             владелец известен, но цель вне конфигурации/неполного дампа — снимать \
             нечего.",
            scan.template_call_sites_missing_target
        ));
    }
    if scan.common_module_literal_sites_missing_target > 0 {
        hint.push_str(&format!(
            " {} мест вызова ОбщийМодуль(\"Имя\") указывают на общий модуль, которого \
             нет в реестре объектов (common_module_literal_sites_missing_target): имя \
             известно из литерала, но цель вне конфигурации/неполного дампа — \
             снимать нечего.",
            scan.common_module_literal_sites_missing_target
        ));
    }
    hint
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        for ddl in crate::schema::SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        conn
    }

    fn add_object(conn: &Connection, full_name: &str, meta_type: &str, name: &str) {
        conn.execute(
            "INSERT INTO metadata_objects (repo, full_name, meta_type, name) \
             VALUES ('default', ?1, ?2, ?3)",
            params![full_name, meta_type, name],
        )
        .unwrap();
        crate::schema::backfill_metadata_object_keys(conn).unwrap();
    }

    fn add_usage(
        conn: &Connection,
        kind: &str,
        object_ref: &str,
        key: &str,
        member: Option<&str>,
        file: &str,
    ) {
        conn.execute(
            "INSERT INTO metadata_code_usages \
             (repo, object_ref, object_ref_key, member_path, usage_kind, file_path, line) \
             VALUES ('default', ?1, ?2, ?3, ?4, ?5, 1)",
            params![object_ref, key, member, kind, file],
        )
        .unwrap();
    }

    fn add_module(conn: &Connection, code_path: &str, object_name: &str) {
        conn.execute(
            "INSERT INTO metadata_modules \
             (repo, full_name, object_name, module_type, object_id, property_id, \
              config_version, code_path, extension_name) \
             VALUES ('default', ?1, ?2, 'ManagerModule', 'uuid', 'prop', NULL, ?3, '')",
            params![
                format!("{}.ManagerModule", object_name),
                object_name,
                code_path
            ],
        )
        .unwrap();
    }

    fn result_names(scan: &Scan) -> Vec<String> {
        scan.results
            .iter()
            .filter_map(|r| r["full_name"].as_str().map(String::from))
            .collect()
    }

    /// Оракул приёмки: используемый через строковый путь макет уходит из
    /// списка, неиспользуемый — остаётся.
    #[test]
    fn template_used_by_string_path_is_not_candidate() {
        let conn = mem();
        add_object(&conn, "Catalog.Вакансии", "Catalog", "Вакансии");
        add_object(
            &conn,
            "Catalog.Вакансии.Template.ПФ_MXL_Вакансия",
            "Template",
            "ПФ_MXL_Вакансия",
        );
        add_object(
            &conn,
            "Catalog.Новости.Template._СтандартныеЗначения_ЗдесьУказатьКодКанала",
            "Template",
            "_СтандартныеЗначения_ЗдесьУказатьКодКанала",
        );
        // Строка запроса: пара (владелец, макет) — как её кладёт extract_code_usages.
        add_usage(
            &conn,
            "query",
            "Catalog.Вакансии",
            "catalog.вакансии",
            Some("ПФ_MXL_Вакансия"),
            "Catalogs/Вакансии/Ext/ManagerModule.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        let names = result_names(&scan);
        assert!(
            !names.contains(&"Catalog.Вакансии.Template.ПФ_MXL_Вакансия".to_string()),
            "используемый макет обязан быть снят каналом строковых путей: {names:?}"
        );
        assert!(
            names.contains(
                &"Catalog.Новости.Template._СтандартныеЗначения_ЗдесьУказатьКодКанала".to_string()
            ),
            "неиспользуемый макет обязан остаться: {names:?}"
        );
        assert_eq!(scan.channels[3], 1, "канал строковых путей снял 1 объект");
        assert_eq!(scan.by_meta_type["Template"], json!(1));
    }

    #[test]
    fn data_links_role_rights_and_code_usages_channels_remove_objects() {
        let conn = mem();
        add_object(&conn, "Catalog.ПоСсылке", "Catalog", "ПоСсылке");
        add_object(&conn, "Catalog.ПоПравам", "Catalog", "ПоПравам");
        add_object(&conn, "Catalog.ПоКоду", "Catalog", "ПоКоду");
        add_object(&conn, "Catalog.Нигде", "Catalog", "Нигде");
        conn.execute(
            "INSERT INTO data_links \
             (repo, from_object, from_path, to_object, link_kind, to_object_key) \
             VALUES ('default','Document.X','Реквизит','Catalog.ПоСсылке','attr','catalog.поссылке')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO role_rights (repo, role_name, object_name, right_name, object_name_key) \
             VALUES ('default','Роль','Catalog.ПоПравам','Read','catalog.поправам')",
            [],
        )
        .unwrap();
        add_usage(
            &conn,
            "manager",
            "Catalog.ПоКоду",
            "catalog.покоду",
            None,
            "Catalogs/ПоКоду/Ext/ManagerModule.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert_eq!(result_names(&scan), vec!["Catalog.Нигде".to_string()]);
        assert_eq!(scan.channels[0], 1, "data_links");
        assert_eq!(scan.channels[1], 1, "role_rights");
        assert_eq!(scan.channels[2], 1, "code_usages");
    }

    #[test]
    fn template_call_resolves_owner_via_metadata_modules() {
        let conn = mem();
        add_object(
            &conn,
            "Catalog.Вакансии.Template.ПФ_MXL_Вакансия",
            "Template",
            "ПФ_MXL_Вакансия",
        );
        add_module(
            &conn,
            "Catalogs/Вакансии/Ext/ManagerModule.bsl",
            "Catalogs.Вакансии",
        );
        add_usage(
            &conn,
            "template_call",
            "Template",
            "template",
            Some("ПФ_MXL_Вакансия"),
            "Catalogs/Вакансии/Ext/ManagerModule.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert!(
            result_names(&scan).is_empty(),
            "владелец из модуля снимает макет"
        );
        assert_eq!(scan.channels[4], 1, "template_calls");
        assert_eq!(scan.template_call_sites_unresolved, 0);
    }

    #[test]
    fn template_call_in_form_module_resolves_to_owner_object() {
        let conn = mem();
        add_object(
            &conn,
            "Catalog.Вакансии.Template.МакетФормы",
            "Template",
            "МакетФормы",
        );
        add_module(
            &conn,
            "Catalogs/Вакансии/Forms/ФормаЭлемента/Ext/Form/Module.bsl",
            "Catalogs.Вакансии.Form.ФормаЭлемента",
        );
        add_usage(
            &conn,
            "template_call",
            "Template",
            "template",
            Some("МакетФормы"),
            "Catalogs/Вакансии/Forms/ФормаЭлемента/Ext/Form/Module.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert!(
            result_names(&scan).is_empty(),
            "макет формы принадлежит самому объекту: {:?}",
            result_names(&scan)
        );
        assert_eq!(scan.channels[4], 1);
    }

    #[test]
    fn template_call_falls_back_to_module_path_without_metadata_modules() {
        let conn = mem();
        add_object(
            &conn,
            "Catalog.Вакансии.Template.МакетПути",
            "Template",
            "МакетПути",
        );
        add_usage(
            &conn,
            "template_call",
            "Template",
            "template",
            Some("МакетПути"),
            "base/Catalogs/Вакансии/Ext/ObjectModule.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert!(
            result_names(&scan).is_empty(),
            "фолбэк по пути <Папка>/<Имя>/… обязан снять макет"
        );
        assert_eq!(scan.channels[4], 1);
        assert_eq!(scan.template_call_sites_unresolved, 0);
    }

    #[test]
    fn unresolved_template_call_is_counted_and_does_not_remove_candidate() {
        let conn = mem();
        add_object(
            &conn,
            "Catalog.Вакансии.Template.Загадка",
            "Template",
            "Загадка",
        );
        add_usage(
            &conn,
            "template_call",
            "Template",
            "template",
            Some("Загадка"),
            "Somewhere/Module.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert_eq!(
            result_names(&scan),
            vec!["Catalog.Вакансии.Template.Загадка".to_string()],
            "нерезолвленный вызов не снимает кандидатуру"
        );
        assert_eq!(scan.template_call_sites_unresolved, 1);
        assert_eq!(scan.channels[4], 0);
    }

    #[test]
    fn common_template_call_removes_only_matching_template() {
        let conn = mem();
        add_object(&conn, "CommonTemplate.Макет", "CommonTemplate", "Макет");
        add_object(&conn, "CommonTemplate.Другой", "CommonTemplate", "Другой");
        add_usage(
            &conn,
            "common_template_call",
            "CommonTemplate",
            "commontemplate",
            Some("Макет"),
            "CommonModules/X/Ext/Module.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert_eq!(
            result_names(&scan),
            vec!["CommonTemplate.Другой".to_string()]
        );
        assert_eq!(scan.channels[5], 1);
    }

    /// A1: метод-форма с менеджер-приёмником снимает макет владельца сразу,
    /// без резолва по файлу (владелец известен из `object_ref`).
    #[test]
    fn manager_template_call_removes_owner_template() {
        let conn = mem();
        add_object(&conn, "Catalog.Вакансии", "Catalog", "Вакансии");
        add_object(
            &conn,
            "Catalog.Вакансии.Template.ПФ_MXL_Вакансия",
            "Template",
            "ПФ_MXL_Вакансия",
        );
        add_usage(
            &conn,
            "template_call_via_manager",
            "Catalog.Вакансии",
            "catalog.вакансии",
            Some("ПФ_MXL_Вакансия"),
            "Catalogs/Вакансии/Ext/ManagerModule.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert!(
            result_names(&scan).is_empty(),
            "метод-форма обязана снять макет владельца: {:?}",
            result_names(&scan)
        );
        assert_eq!(scan.channels[6], 1, "template_calls_via_manager");
    }

    /// A1/A2: метод-форма с нераспознанным приёмником НЕ снимает макет, но
    /// попадает в счётчик непокрытой формы.
    #[test]
    fn manager_receiver_unknown_is_counted_not_removed() {
        let conn = mem();
        add_object(
            &conn,
            "Catalog.Вакансии.Template.Загадка",
            "Template",
            "Загадка",
        );
        add_usage(
            &conn,
            "template_call_via_receiver",
            "Template",
            "template",
            Some("Загадка"),
            "Somewhere/Module.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert_eq!(
            result_names(&scan),
            vec!["Catalog.Вакансии.Template.Загадка".to_string()],
            "непокрытая форма не снимает кандидатуру"
        );
        assert_eq!(scan.template_call_sites_uncovered, 1);
        assert_eq!(scan.channels[6], 0);
        assert!(
            !scan
                .channels_not_applied
                .iter()
                .any(|c| c == "template_calls_not_indexed"),
            "форма материализована счётчиком — флага not_indexed быть не должно"
        );
    }

    /// A9: имя существует только у чужого владельца — вызов из модуля B не
    /// снимает макет объекта A (cross-owner).
    #[test]
    fn foreign_owner_template_call_does_not_remove() {
        let conn = mem();
        add_object(&conn, "Catalog.A.Template.X", "Template", "X");
        add_object(&conn, "Catalog.B.Template.Y", "Template", "Y");
        add_module(&conn, "Catalogs/B/Ext/ManagerModule.bsl", "Catalogs.B");
        add_usage(
            &conn,
            "template_call",
            "Template",
            "template",
            Some("X"),
            "Catalogs/B/Ext/ManagerModule.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        let names = result_names(&scan);
        assert!(
            names.contains(&"Catalog.A.Template.X".to_string()),
            "чужой владелец не даёт снять A.X: {names:?}"
        );
        assert!(names.contains(&"Catalog.B.Template.Y".to_string()));
    }

    /// B4(a): у `Catalog.A` и `Catalog.B` есть макеты с ОДИНАКОВЫМ именем `X`.
    /// Метод-форма `Справочники.B.ПолучитьМакет("X")` снимает ТОЛЬКО
    /// `Catalog.B.Template.X`, а `Catalog.A.Template.X` остаётся в results.
    #[test]
    fn manager_template_call_removes_only_matching_owner_on_name_collision() {
        let conn = mem();
        add_object(&conn, "Catalog.A.Template.X", "Template", "X");
        add_object(&conn, "Catalog.B.Template.X", "Template", "X");
        add_usage(
            &conn,
            "template_call_via_manager",
            "Catalog.B",
            "catalog.b",
            Some("X"),
            "Catalogs/B/Ext/ManagerModule.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        let names = result_names(&scan);
        assert!(
            !names.contains(&"Catalog.B.Template.X".to_string()),
            "владелец из приёмника обязан снять B.X: {names:?}"
        );
        assert!(
            names.contains(&"Catalog.A.Template.X".to_string()),
            "одноимённый макет чужого владельца обязан остаться: {names:?}"
        );
        assert_eq!(scan.channels[6], 1, "template_calls_via_manager");
    }

    /// B4(a): та же коллизия для ГОЛОЙ формы `ПолучитьМакет("X")` — владелец
    /// резолвится по модулю вызывающего; снимается только макет владельца B.
    #[test]
    fn bare_template_call_removes_only_matching_owner_on_name_collision() {
        let conn = mem();
        add_object(&conn, "Catalog.A.Template.X", "Template", "X");
        add_object(&conn, "Catalog.B.Template.X", "Template", "X");
        add_module(&conn, "Catalogs/B/Ext/ManagerModule.bsl", "Catalogs.B");
        add_usage(
            &conn,
            "template_call",
            "Template",
            "template",
            Some("X"),
            "Catalogs/B/Ext/ManagerModule.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        let names = result_names(&scan);
        assert!(
            !names.contains(&"Catalog.B.Template.X".to_string()),
            "владелец по модулю обязан снять B.X: {names:?}"
        );
        assert!(
            names.contains(&"Catalog.A.Template.X".to_string()),
            "одноимённый макет чужого владельца обязан остаться: {names:?}"
        );
        assert_eq!(scan.channels[4], 1, "template_calls");
    }

    /// B4(b): пары менеджер-канала собираются чанками `OWNER_CHUNK`;
    /// совпадение из второго чанка (владельцев > OWNER_CHUNK) обязано снять
    /// макет владельца. Имена владельцев — с ведущими нулями, чтобы порядок
    /// объектов в базе (лексикографический) совпал с числовым: тогда цель
    /// (`O0400`) гарантированно попадает во ВТОРОЙ чанк.
    #[test]
    fn manager_template_pairs_span_owner_chunks_over_400() {
        let conn = mem();
        let n = OWNER_CHUNK + 1; // 401 владелец — гарантированно два чанка
        for i in 0..n {
            add_object(
                &conn,
                &format!("Catalog.O{i:04}.Template.M{i:04}"),
                "Template",
                &format!("M{i:04}"),
            );
        }
        let last = n - 1;
        add_usage(
            &conn,
            "template_call_via_manager",
            &format!("Catalog.O{last:04}"),
            &format!("catalog.o{last:04}"),
            Some(&format!("M{last:04}")),
            "Catalogs/O/Ext/ManagerModule.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        let names = result_names(&scan);
        assert!(
            !names.contains(&format!("Catalog.O{last:04}.Template.M{last:04}")),
            "пара из второго чанка менеджер-канала обязана снять макет"
        );
        assert_eq!(scan.channels[6], 1);
        assert_eq!(names.len(), n - 1);
    }

    /// A2/A11: непокрытые формы всегда перечислены в `channels_not_applied`;
    /// на пустой базе флага `template_calls_not_indexed` нет.
    #[test]
    fn channels_not_applied_lists_uncovered_forms() {
        let conn = mem();
        let scan = scan_repository(&conn, None).unwrap();
        for form in NOT_INDEXED_TEMPLATE_FORMS {
            assert!(
                scan.channels_not_applied.iter().any(|c| c == form),
                "форма {form} обязана быть в channels_not_applied: {:?}",
                scan.channels_not_applied
            );
        }
        assert!(
            !scan
                .channels_not_applied
                .iter()
                .any(|c| c == "template_calls_not_indexed"),
            "пустая таблица — судить не о чем"
        );
    }

    /// B2: строки есть, но ни одной вида `template_call*` — полнота канала
    /// макетов не подтверждена, оговорка в hint обязательна. Причина из данных
    /// неразличима, поэтому hint не утверждает «индекс собран прежним
    /// бинарником» как факт, а перечисляет обе возможности.
    #[test]
    fn index_without_template_call_kinds_is_flagged_neutrally() {
        let conn = mem();
        add_object(
            &conn,
            "Catalog.Вакансии.Template.Макет",
            "Template",
            "Макет",
        );
        add_usage(
            &conn,
            "query",
            "Catalog.Вакансии",
            "catalog.вакансии",
            Some("Другой"),
            "Catalogs/Вакансии/Ext/ManagerModule.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert!(
            scan.channels_not_applied
                .iter()
                .any(|c| c == "template_calls_not_indexed"),
            "{:?}",
            scan.channels_not_applied
        );
        let hint = build_hint(&scan, None);
        assert!(
            hint.contains("полнота канала макетов не подтверждена"),
            "{hint}"
        );
        assert!(hint.contains("неразличима"), "{hint}");
        assert!(hint.contains("нет ни одного вызова"), "{hint}");
        assert!(hint.contains("template_calls_not_indexed"), "{hint}");
        // Прежняя (B2) формулировка «индекс собран прежним бинарником»
        // утверждала причину как факт — её быть не должно.
        assert!(
            !hint.contains("индекс собран прежним бинарником"),
            "причина не должна утверждаться как факт: {hint}"
        );
    }

    /// A4 (обратная сторона): после переиндексации новым бинарником флага нет.
    #[test]
    fn materialized_index_has_no_not_indexed_flag() {
        let conn = mem();
        add_object(
            &conn,
            "Catalog.Вакансии.Template.Макет",
            "Template",
            "Макет",
        );
        add_usage(
            &conn,
            "template_call",
            "Template",
            "template",
            Some("Макет"),
            "Catalogs/Вакансии/Ext/ManagerModule.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert!(
            !scan
                .channels_not_applied
                .iter()
                .any(|c| c == "template_calls_not_indexed"),
            "{:?}",
            scan.channels_not_applied
        );
    }

    /// A9: пары владельцев собираются чанками `OWNER_CHUNK`; совпадение из
    /// второго чанка обязано снять макет.
    #[test]
    fn template_path_pairs_span_owner_chunks_over_400() {
        let conn = mem();
        let n = OWNER_CHUNK + 1; // 401 владелец — гарантированно два чанка
        for i in 0..n {
            add_object(
                &conn,
                &format!("Catalog.O{i}.Template.M{i}"),
                "Template",
                &format!("M{i}"),
            );
        }
        let last = n - 1;
        add_usage(
            &conn,
            "query",
            &format!("Catalog.O{last}"),
            &format!("catalog.o{last}"),
            Some(&format!("M{last}")),
            "Catalogs/O/Ext/ManagerModule.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        let names = result_names(&scan);
        assert!(
            !names.contains(&format!("Catalog.O{last}.Template.M{last}")),
            "пара из второго чанка обязана снять макет"
        );
        assert_eq!(scan.channels[3], 1);
        assert_eq!(names.len(), n - 1);
    }

    /// A2: hint упоминает непокрытую форму счётчиком.
    #[test]
    fn hint_mentions_uncovered_receiver_calls() {
        let conn = mem();
        add_object(
            &conn,
            "Catalog.Вакансии.Template.Загадка",
            "Template",
            "Загадка",
        );
        add_usage(
            &conn,
            "template_call_via_receiver",
            "Template",
            "template",
            Some("Загадка"),
            "Somewhere/Module.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        let hint = build_hint(&scan, None);
        assert!(hint.contains("нераспознанным приёмником"), "{hint}");
    }

    /// A9: ветка hint про нерезолвленный вызов (владелец не опознан).
    #[test]
    fn hint_mentions_unresolved_template_calls() {
        let conn = mem();
        add_object(
            &conn,
            "Catalog.Вакансии.Template.Загадка",
            "Template",
            "Загадка",
        );
        add_usage(
            &conn,
            "template_call",
            "Template",
            "template",
            Some("Загадка"),
            "Somewhere/Module.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert_eq!(scan.template_call_sites_unresolved, 1);
        let hint = build_hint(&scan, None);
        assert!(hint.contains("не привязано к владельцу"), "{hint}");
    }

    /// A9: ветка hint про нулевые счётчики каналов макетов при материализованном
    /// канале (kinds есть, но ни одна пара не сняла объект).
    #[test]
    fn hint_mentions_zero_template_counters_with_materialized_channel() {
        let conn = mem();
        add_object(
            &conn,
            "Catalog.Вакансии.Template.Несвязанный",
            "Template",
            "Несвязанный",
        );
        // Строка нового вида есть (канал «материализован»), но она — для другого
        // приёмника/макета и кандидата не снимает.
        add_usage(
            &conn,
            "template_call_via_receiver",
            "Template",
            "template",
            Some("Другой"),
            "Somewhere/Module.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert!(scan.channels[3..].iter().all(|c| *c == 0));
        let hint = build_hint(&scan, None);
        assert!(hint.contains("Нулевые счётчики каналов макетов"), "{hint}");
    }

    #[test]
    fn subsystem_and_language_are_not_addressable() {
        let conn = mem();
        add_object(&conn, "Subsystem.Продажи", "Subsystem", "Продажи");
        add_object(&conn, "Language.Русский", "Language", "Русский");
        add_object(&conn, "Catalog.Нигде", "Catalog", "Нигде");

        let scan = scan_repository(&conn, None).unwrap();
        assert_eq!(result_names(&scan), vec!["Catalog.Нигде".to_string()]);
        assert_eq!(scan.not_addressable["Subsystem"], json!(1));
        assert_eq!(scan.not_addressable["Language"], json!(1));
    }

    #[test]
    fn meta_type_filter_narrows_results_and_counters() {
        let conn = mem();
        add_object(&conn, "Template.Один", "Template", "Один");
        add_object(&conn, "Subsystem.Два", "Subsystem", "Два");

        let scan = scan_repository(&conn, Some("Template")).unwrap();
        assert_eq!(result_names(&scan), vec!["Template.Один".to_string()]);
        assert_eq!(scan.by_meta_type["Template"], json!(1));
        assert!(
            scan.not_addressable.is_empty(),
            "вне фильтра Subsystem не считается"
        );

        // RU-форма нормализована на входе execute; scan принимает канон.
        let scan = scan_repository(&conn, Some("Subsystem")).unwrap();
        assert!(result_names(&scan).is_empty());
        assert_eq!(scan.not_addressable["Subsystem"], json!(1));
    }

    #[test]
    fn pagination_marks_keep_total_and_next_offset() {
        let conn = mem();
        for i in 0..120 {
            add_object(
                &conn,
                &format!("Catalog.Объект.Template.Макет{}", i),
                "Template",
                &format!("Макет{}", i),
            );
        }
        let scan = scan_repository(&conn, Some("Template")).unwrap();
        assert_eq!(scan.results.len(), 120);

        let page = cap::page_by_bytes(scan.results.clone(), 0, 3_000);
        assert!(page.shown > 0 && page.shown < 120, "страница по бюджету");
        assert_eq!(page.total, 120);
        assert!(page.has_more);
        let next = page.next_offset().unwrap();
        assert_eq!(next, page.shown);

        let page2 = cap::page_by_bytes(scan.results, next, 3_000);
        assert_eq!(page2.offset, next);
        assert!(page2.shown > 0);
        assert_eq!(page2.total, 120);
    }

    /// B1: итоговый JSON ответа (вместе с обёрткой `{result,_meta}`) не
    /// превышает ЗАПРОШЕННЫЙ бюджет. Выросшая фиксированная шапка (A2/A4)
    /// раньше выносила ответ за бюджет, т.к. `page_by_bytes` резервировал
    /// константный `PAGE_OVERHEAD_BYTES`, а не фактический конверт.
    #[test]
    fn wrapped_response_fits_requested_budget() {
        // Элементы с кириллицей (2 байта/символ), набор заведомо больше бюджета:
        // каждая страница заполняется целиком.
        let results: Vec<Value> = (0..1000)
            .map(|i| {
                json!({
                    "full_name": format!("Catalog.НекоторыйОбъектНомер{i:04}"),
                    "meta_type": "Catalog",
                    "name": format!("НекоторыйОбъектНомер{i:04}"),
                })
            })
            .collect();
        let scan = Scan {
            results: results.clone(),
            by_meta_type: Map::new(),
            channels: [3, 2, 1, 5, 4, 6, 7, 8, 9, 10, 11, 12],
            not_addressable: Map::new(),
            template_call_sites_unresolved: 2,
            template_call_sites_missing_target: 3,
            common_module_literal_sites_missing_target: 2,
            template_call_sites_uncovered: 1,
            channels_not_applied: NOT_INDEXED_TEMPLATE_FORMS
                .iter()
                .map(|s| s.to_string())
                .chain(std::iter::once("template_calls_not_indexed".to_string()))
                .collect(),
        };
        // Бюджеты, на которых конверт (шапка + hint + обёртка) помещается в
        // бюджет: итог обязан уложиться.
        for budget in [48_000usize, 8_000, 192_000, 70_000] {
            let out = page_and_wrap(results.clone(), &scan, None, "default", 0, budget);
            let len = serde_json::to_string(&out).unwrap().len();
            println!(
                "B1 budget={budget} -> len={len} shown={} total={}",
                out["result"]["results_shown"], out["result"]["results_total"]
            );
            assert!(
                len <= budget,
                "бюджет {budget}: итоговый размер {len} превысил бюджет"
            );
            let shown = out["result"]["results"]
                .as_array()
                .expect("results — массив");
            assert!(!shown.is_empty(), "массив results не обнуляется");
            assert_eq!(
                out["result"]["results_shown"].as_u64(),
                Some(shown.len() as u64)
            );
            assert_eq!(
                out["result"]["results_has_more"].as_bool(),
                Some(shown.len() < scan.results.len())
            );
            assert!(out["_meta"]["dependent_files"].is_array());
        }

        // Бюджет НИЖЕ размера обязательного конверта: уложиться нельзя ни при
        // каком наборе (шапка каналов, `channels_not_applied` и hint обязательны
        // по A2/B2/B3 и не зависят от `results`). Инструмент обязан отдать
        // МИНИМУМ — конверт + ровно 1 элемент, — а не обнулить массив.
        let min_at_1 = serde_json::to_string(&page_and_wrap(
            results.clone(),
            &scan,
            None,
            "default",
            0,
            1,
        ))
        .unwrap()
        .len();
        let envelope_only =
            serde_json::to_string(&page_and_wrap(Vec::new(), &scan, None, "default", 0, 1))
                .unwrap()
                .len();
        println!("B1 envelope(empty results, budget=1)={envelope_only} min_len={min_at_1}");
        assert!(
            envelope_only > 1_000,
            "конверт обязан быть больше 1000 Б (иначе тест 1000 был бы выполним)"
        );
        for budget in [1_000usize, 3_000] {
            let out = page_and_wrap(results.clone(), &scan, None, "default", 0, budget);
            let len = serde_json::to_string(&out).unwrap().len();
            println!("B1 tiny budget={budget} -> len={len}");
            assert_eq!(
                len, min_at_1,
                "при бюджете {budget} ниже конверта отдаётся ровно минимум (конверт + 1 элемент)"
            );
            let shown = out["result"]["results"]
                .as_array()
                .expect("results — массив");
            assert_eq!(shown.len(), 1, "страница не обнуляется: ровно 1 элемент");
        }
    }

    #[test]
    fn owner_key_helpers_cover_dump_layouts() {
        assert_eq!(
            owner_key_from_module_object_name("Catalogs.Вакансии").as_deref(),
            Some("catalog.вакансии")
        );
        assert_eq!(
            owner_key_from_module_object_name("Catalogs.Вакансии.Form.ФормаЭлемента").as_deref(),
            Some("catalog.вакансии")
        );
        assert_eq!(
            owner_key_from_module_object_name("Catalogs.Вакансии.Command.Команда1").as_deref(),
            Some("catalog.вакансии")
        );
        assert_eq!(
            owner_key_from_path("base/Catalogs/Вакансии/Ext/ManagerModule.bsl").as_deref(),
            Some("catalog.вакансии")
        );
        assert_eq!(
            owner_key_from_path("src/Catalogs/Вакансии/ObjectModule.bsl").as_deref(),
            Some("catalog.вакансии")
        );
        assert_eq!(
            owner_key_from_path("CommonModules/Общий/Ext/Module.bsl").as_deref(),
            Some("commonmodule.общий")
        );
        assert_eq!(owner_key_from_path("Somewhere/Module.bsl"), None);
        assert_eq!(owner_key_from_module_object_name("Загадка"), None);
    }

    /// Инструмент не должен попадать под `cap_response`: массив `results` —
    /// не сэмпл, а страница, собранная по бюджету; повторный слепой обрез
    /// исказил бы `results_total`/`results_has_more`.
    #[test]
    fn tool_is_not_capped_by_default() {
        cap::set_cap_tools(None);
        cap::set_cap_enabled(Some(true));
        assert!(!cap::cap_applies("find_unreferenced_objects"));
        assert!(cap::cap_applies("find_references"));
    }

    /// D1.1/D1.2: обращения к общим картинкам и элементам стиля (из кода и XML
    /// форм) снимают объекты своими каналами; неиспользуемые остаются.
    #[test]
    fn common_picture_and_style_item_usages_remove_objects() {
        let conn = mem();
        add_object(&conn, "CommonPicture.Папка", "CommonPicture", "Папка");
        add_object(
            &conn,
            "CommonPicture.НетСсылок",
            "CommonPicture",
            "НетСсылок",
        );
        add_object(&conn, "StyleItem.ЦветТекста", "StyleItem", "ЦветТекста");
        add_object(&conn, "StyleItem.НетСсылок", "StyleItem", "НетСсылок");
        add_usage(
            &conn,
            "common_picture",
            "CommonPicture.Папка",
            "commonpicture.папка",
            None,
            "Catalogs/X/Forms/F/Ext/Form.xml",
        );
        add_usage(
            &conn,
            "style_item",
            "StyleItem.ЦветТекста",
            "styleitem.цветтекста",
            None,
            "Catalogs/X/Forms/F/Ext/Form.xml",
        );

        let scan = scan_repository(&conn, None).unwrap();
        let names = result_names(&scan);
        assert!(
            !names.contains(&"CommonPicture.Папка".to_string()),
            "{names:?}"
        );
        assert!(
            names.contains(&"CommonPicture.НетСсылок".to_string()),
            "{names:?}"
        );
        assert!(
            !names.contains(&"StyleItem.ЦветТекста".to_string()),
            "{names:?}"
        );
        assert!(
            names.contains(&"StyleItem.НетСсылок".to_string()),
            "{names:?}"
        );
        assert_eq!(scan.channels[7], 1, "common_pictures");
        assert_eq!(scan.channels[8], 1, "style_items");
    }

    /// D1.3: общий модуль, чьи вызовы адресованы в `proc_call_graph`
    /// (`callee_proc_key = <code_path>::<метод>`), снимается каналом общих
    /// модулей без переиндексации кода; невызываемый остаётся.
    #[test]
    fn common_module_used_by_call_graph_is_removed() {
        let conn = mem();
        add_object(&conn, "CommonModule.Общий", "CommonModule", "Общий");
        add_object(&conn, "CommonModule.Мёртвый", "CommonModule", "Мёртвый");
        add_module(
            &conn,
            "CommonModules/Общий/Ext/Module.bsl",
            "CommonModules.Общий",
        );
        conn.execute(
            "INSERT INTO proc_call_graph \
             (repo, caller_proc_key, callee_proc_name, callee_proc_key, call_type) \
             VALUES ('default','Other/Module.bsl::A','Метод',\
                     'CommonModules/Общий/Ext/Module.bsl::Метод','direct')",
            [],
        )
        .unwrap();

        let scan = scan_repository(&conn, None).unwrap();
        let names = result_names(&scan);
        assert!(
            !names.contains(&"CommonModule.Общий".to_string()),
            "использованный общий модуль обязан быть снят: {names:?}"
        );
        assert!(
            names.contains(&"CommonModule.Мёртвый".to_string()),
            "невызываемый общий модуль обязан остаться: {names:?}"
        );
        assert_eq!(scan.channels[9], 1, "common_modules");
    }

    /// D3: цель метод-формы вне реестра считается отдельно и не путается с
    /// «владелец не выведен».
    #[test]
    fn missing_target_sites_are_counted_separately() {
        let conn = mem();
        add_object(&conn, "Catalog.Вакансии.Template.ПФ", "Template", "ПФ");
        add_usage(
            &conn,
            "template_call_via_manager",
            "Catalog.Вакансии",
            "catalog.вакансии",
            Some("ПФ"),
            "Catalogs/Вакансии/Ext/ManagerModule.bsl",
        );
        // Цель отсутствует в реестре: владелец есть, макета нет.
        add_usage(
            &conn,
            "template_call_via_manager",
            "Catalog.Нет",
            "catalog.нет",
            Some("Пропал"),
            "Catalogs/Нет/Ext/ManagerModule.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert_eq!(scan.channels[6], 1, "снят только существующий макет");
        assert_eq!(scan.template_call_sites_missing_target, 1);
        assert_eq!(scan.template_call_sites_unresolved, 0);
        let hint = build_hint(&scan, None);
        assert!(
            hint.contains("template_call_sites_missing_target"),
            "{hint}"
        );
    }

    /// D3 (строковый канал модулей): `ОбщийМодуль("Имя")`, где
    /// `CommonModule.<Имя>` отсутствует в реестре, считается отдельно — аналог
    /// `template_call_sites_missing_target` (единица — МЕСТА вызова).
    #[test]
    fn common_module_literal_missing_target_sites_are_counted() {
        let conn = mem();
        add_object(&conn, "CommonModule.Обмен", "CommonModule", "Обмен");
        add_usage(
            &conn,
            "common_module_via_literal",
            "CommonModule.Обмен",
            "commonmodule.обмен",
            None,
            "CommonModules/Х/Ext/Module.bsl",
        );
        // Цель вне реестра: строка есть, объекта `CommonModule.Пропал` нет.
        add_usage(
            &conn,
            "common_module_via_literal",
            "CommonModule.Пропал",
            "commonmodule.пропал",
            None,
            "CommonModules/Х/Ext/Module.bsl",
        );

        let scan = scan_repository(&conn, None).unwrap();
        assert_eq!(scan.channels[11], 1, "снят только модуль из реестра");
        assert_eq!(scan.common_module_literal_sites_missing_target, 1);
        assert!(
            result_names(&scan).is_empty(),
            "модуль из реестра снят обращением, а цель вне реестра в results не попадает"
        );
        let hint = build_hint(&scan, None);
        assert!(
            hint.contains("common_module_literal_sites_missing_target"),
            "{hint}"
        );
    }

    /// D1.3 (оговорка): на индексе без адресов вызовов и без строк модулей
    /// канал общих модулей перечисляется в `channels_not_applied`; на пустой
    /// базе оговорки нет (судить не о чем).
    #[test]
    fn common_module_channel_flagged_when_data_absent() {
        // Пустая база → оговорок по новым каналам нет.
        let empty = mem();
        let scan = scan_repository(&empty, None).unwrap();
        assert!(
            !scan
                .channels_not_applied
                .iter()
                .any(|c| c == "common_module_calls_not_indexed"),
            "{:?}",
            scan.channels_not_applied
        );

        // Есть строки кода, но граф без адресов и модулей нет → оговорка.
        let conn = mem();
        add_object(&conn, "CommonModule.Общий", "CommonModule", "Общий");
        add_usage(
            &conn,
            "manager",
            "Catalog.X",
            "catalog.x",
            None,
            "Catalogs/X/Ext/ManagerModule.bsl",
        );
        conn.execute(
            "INSERT INTO proc_call_graph \
             (repo, caller_proc_key, callee_proc_name, call_type) \
             VALUES ('default','A','B','direct')",
            [],
        )
        .unwrap();
        let scan = scan_repository(&conn, None).unwrap();
        assert!(
            scan.channels_not_applied
                .iter()
                .any(|c| c == "common_module_calls_not_indexed"),
            "{:?}",
            scan.channels_not_applied
        );
        assert!(
            scan.channels_not_applied
                .iter()
                .any(|c| c == "common_picture_usages_not_indexed"),
            "{:?}",
            scan.channels_not_applied
        );
        assert!(
            scan.channels_not_applied
                .iter()
                .any(|c| c == "style_item_usages_not_indexed"),
            "{:?}",
            scan.channels_not_applied
        );
        let hint = build_hint(&scan, None);
        assert!(hint.contains("полнота каналов"), "{hint}");
    }

    /// Ребро графа вызовов с адресом цели.
    fn add_edge(conn: &Connection, caller: &str, callee_name: &str, callee_key: Option<&str>) {
        conn.execute(
            "INSERT INTO proc_call_graph \
             (repo, caller_proc_key, callee_proc_name, callee_proc_key, call_type) \
             VALUES ('default', ?1, ?2, ?3, 'direct')",
            params![caller, callee_name, callee_key],
        )
        .unwrap();
    }

    /// Подписка на событие. `handler_module` — как в `event_subscriptions`
    /// (`CommonModule.X`) либо короткое имя (`X`).
    fn add_subscription(conn: &Connection, name: &str, handler_module: &str, handler_proc: &str) {
        conn.execute(
            "INSERT INTO event_subscriptions \
             (repo, name, event, handler_module, handler_proc, sources_json) \
             VALUES ('default', ?1, 'ПриЗаписи', ?2, ?3, '[]')",
            params![name, handler_module, handler_proc],
        )
        .unwrap();
    }

    /// E2: self-ребро модуля не считается использованием; внешний вызов — считается.
    #[test]
    fn common_module_self_edge_is_not_usage() {
        let conn = mem();
        add_object(&conn, "CommonModule.А", "CommonModule", "А");
        add_module(&conn, "CommonModules/А/Ext/Module.bsl", "CommonModules.А");
        // Внутренний вызов: модуль А вызывает сам себя.
        add_edge(
            &conn,
            "CommonModules/А/Ext/Module.bsl::П",
            "Внутренняя",
            Some("CommonModules/А/Ext/Module.bsl::Внутренняя"),
        );
        let scan = scan_repository(&conn, None).unwrap();
        assert!(
            result_names(&scan).contains(&"CommonModule.А".to_string()),
            "self-ребро не снимает объект: {:?}",
            result_names(&scan)
        );
        assert_eq!(scan.channels[9], 0);

        // Внешний вызов (из другого модуля) снимает объект.
        add_edge(
            &conn,
            "CommonModules/Б/Ext/Module.bsl::П",
            "Внешняя",
            Some("CommonModules/А/Ext/Module.bsl::Внешняя"),
        );
        let scan = scan_repository(&conn, None).unwrap();
        assert!(
            result_names(&scan).is_empty(),
            "внешний вызов снимает объект: {:?}",
            result_names(&scan)
        );
        assert_eq!(scan.channels[9], 1);
    }

    /// E12(d): несколько модулей, модуль расширения, соседний `Module2.bsl` и
    /// объект без модулей; self между двумя модулями одного объекта — всё ещё
    /// self (использованием не является).
    #[test]
    fn common_module_edges_multiple_extension_and_neighbour_paths() {
        let conn = mem();
        add_object(&conn, "CommonModule.А", "CommonModule", "А");
        add_object(&conn, "CommonModule.Б", "CommonModule", "Б");
        add_object(&conn, "CommonModule.В", "CommonModule", "В");
        // У А два модуля: основной и в расширении (extension_name различает ключ).
        add_module(&conn, "CommonModules/А/Ext/Module.bsl", "CommonModules.А");
        conn.execute(
            "INSERT INTO metadata_modules \
             (repo, full_name, object_name, module_type, object_id, property_id, \
              config_version, code_path, extension_name) \
             VALUES ('default', 'CommonModules.А.ManagerModule', 'CommonModules.А', \
                     'ManagerModule', 'uuid2', 'prop', NULL, \
                     'extensions/EF/CommonModules/А/Ext/Module.bsl', 'extensions/EF')",
            [],
        )
        .unwrap();
        add_module(&conn, "CommonModules/Б/Ext/Module.bsl", "CommonModules.Б");
        // У В модулей нет вовсе.

        // Соседний `Module2.bsl`: ребро адресует его, а не `Module.bsl` —
        // префикс `<path>::` не должен ложно снять А.
        add_edge(
            &conn,
            "x.bsl::П",
            "M2",
            Some("CommonModules/А/Ext/Module2.bsl::Внутренняя"),
        );
        // Self между ДВУМЯ модулями одного объекта (основной → расширение).
        add_edge(
            &conn,
            "extensions/EF/CommonModules/А/Ext/Module.bsl::П",
            "S",
            Some("CommonModules/А/Ext/Module.bsl::S"),
        );
        let scan = scan_repository(&conn, None).unwrap();
        let names = result_names(&scan);
        assert!(
            names.contains(&"CommonModule.А".to_string()),
            "self/соседний путь не снимают: {names:?}"
        );
        assert!(
            names.contains(&"CommonModule.В".to_string()),
            "объект без модулей остаётся: {names:?}"
        );

        // Внешнее ребро на модуль расширения снимает А.
        add_edge(
            &conn,
            "CommonModules/Б/Ext/Module.bsl::П",
            "E",
            Some("extensions/EF/CommonModules/А/Ext/Module.bsl::E"),
        );
        let scan = scan_repository(&conn, None).unwrap();
        assert!(
            !result_names(&scan).contains(&"CommonModule.А".to_string()),
            "внешнее ребро на модуль расширения снимает объект"
        );

        // Внешнее ребро на основной модуль Б — снимает и его.
        add_edge(
            &conn,
            "CommonModules/А/Ext/Module.bsl::П",
            "B2",
            Some("CommonModules/Б/Ext/Module.bsl::B2"),
        );
        let scan = scan_repository(&conn, None).unwrap();
        let names = result_names(&scan);
        assert!(!names.contains(&"CommonModule.А".to_string()), "{names:?}");
        assert!(!names.contains(&"CommonModule.Б".to_string()), "{names:?}");
        assert!(names.contains(&"CommonModule.В".to_string()), "{names:?}");
    }

    /// E6: путь модуля может содержать `::` — адрес вызова разбирается по
    /// ПОСЛЕДНЕМУ разделителю (`rfind`), а не `split_once`.
    #[test]
    fn common_module_path_with_double_colon_is_matched() {
        let conn = mem();
        add_object(&conn, "CommonModule.А", "CommonModule", "А");
        let path = "base/CommonModules/А::v2/Ext/Module.bsl";
        add_module(&conn, path, "CommonModules.А");
        let callee_key = format!("{path}::Метод");
        add_edge(
            &conn,
            "CommonModules/Б/Ext/Module.bsl::П",
            "M",
            Some(&callee_key),
        );
        let scan = scan_repository(&conn, None).unwrap();
        assert!(
            !result_names(&scan).contains(&"CommonModule.А".to_string()),
            "путь с `::` сопоставляется целиком: {:?}",
            result_names(&scan)
        );
    }

    /// F1a: модуль, использованный ТОЛЬКО как обработчик подписки, не считается
    /// мёртвым: ребра вызова у подписки нет (`callee_proc_key IS NULL` — там имя
    /// процедуры), спасает отдельный источник канала
    /// (`event_subscriptions.handler_module`).
    #[test]
    fn common_module_used_only_by_subscription_is_not_candidate() {
        let conn = mem();
        add_object(
            &conn,
            "CommonModule.Обработчик",
            "CommonModule",
            "Обработчик",
        );
        add_object(&conn, "CommonModule.Нигде", "CommonModule", "Нигде");
        add_module(
            &conn,
            "CommonModules/Обработчик/Ext/Module.bsl",
            "CommonModules.Обработчик",
        );
        add_module(
            &conn,
            "CommonModules/Нигде/Ext/Module.bsl",
            "CommonModules.Нигде",
        );
        // Обработчик подписки: полное имя модуля (как в XML выгрузки) и короткая
        // форма (канал принимает обе).
        add_subscription(&conn, "П1", "CommonModule.Обработчик", "ПриЗаписиДокумента");
        add_subscription(&conn, "П2", "Обработчик", "ПриЗаписиДокумента");
        // Ребро подписки в графе — БЕЗ адреса цели: канал графа её не видит.
        add_edge(
            &conn,
            "CommonModules/Обработчик/Ext/Module.bsl::П",
            "S",
            None,
        );

        let scan = scan_repository(&conn, None).unwrap();
        let names = result_names(&scan);
        assert!(
            !names.contains(&"CommonModule.Обработчик".to_string()),
            "модуль-обработчик подписки обязан быть снят: {names:?}"
        );
        assert!(
            names.contains(&"CommonModule.Нигде".to_string()),
            "модуль без подписки и без вызовов остаётся кандидатом: {names:?}"
        );
        assert_eq!(scan.channels[9], 0, "граф тут ни при чём");
        assert_eq!(scan.channels[10], 1, "снял канал подписок");
        assert_eq!(scan.channels[11], 0, "строковых обращений нет");
    }

    /// F1a: подписка на модуль, которого нет в `metadata_modules` (индекс без
    /// этого модуля), никого не снимает — канал не выдумывает владельцев.
    #[test]
    fn subscription_to_unknown_module_does_not_remove_anyone() {
        let conn = mem();
        add_object(&conn, "CommonModule.А", "CommonModule", "А");
        add_module(&conn, "CommonModules/А/Ext/Module.bsl", "CommonModules.А");
        add_subscription(&conn, "П", "CommonModule.Чужой", "ПриЗаписиДокумента");
        let scan = scan_repository(&conn, None).unwrap();
        assert!(result_names(&scan).contains(&"CommonModule.А".to_string()));
        assert_eq!(scan.channels[10], 0);
    }

    /// F1b: строковый доступ `ОбщийМодуль("Имя")` — обращение к ОБЪЕКТУ
    /// `CommonModule`; оно снимает модуль и считается своим счётчиком.
    #[test]
    fn common_module_literal_usage_removes_object() {
        let conn = mem();
        add_object(&conn, "CommonModule.Обмен", "CommonModule", "Обмен");
        add_object(&conn, "CommonModule.Нигде", "CommonModule", "Нигде");
        add_usage(
            &conn,
            "common_module_via_literal",
            "CommonModule.Обмен",
            "commonmodule.обмен",
            None,
            "CommonModules/Х/Ext/Module.bsl",
        );
        let scan = scan_repository(&conn, None).unwrap();
        let names = result_names(&scan);
        assert!(
            !names.contains(&"CommonModule.Обмен".to_string()),
            "строковый доступ к модулю обязан быть снят: {names:?}"
        );
        assert!(
            names.contains(&"CommonModule.Нигде".to_string()),
            "{names:?}"
        );
        assert_eq!(scan.channels[11], 1, "счётчик строковых обращений");
        assert_eq!(scan.channels[2], 1, "универсальный code_usages тоже");
    }

    /// F1b (честное имя формы): строк вида `common_module_via_literal` в базе
    /// нет, хотя обращения в коде есть — полнота канала не подтверждена.
    #[test]
    fn common_module_literal_channel_unconfirmed_is_flagged() {
        let conn = mem();
        add_object(&conn, "CommonModule.А", "CommonModule", "А");
        add_object(&conn, "Catalog.Х", "Catalog", "Х");
        add_usage(
            &conn,
            "manager",
            "Catalog.Х",
            "catalog.х",
            None,
            "Catalogs/Х/Ext/ManagerModule.bsl",
        );
        let scan = scan_repository(&conn, None).unwrap();
        assert!(
            scan.channels_not_applied
                .iter()
                .any(|c| c == "common_module_literal_usages_not_indexed"),
            "{:?}",
            scan.channels_not_applied
        );
        let hint = build_hint(&scan, None);
        assert!(
            hint.contains("common_module_literal_usages_not_indexed"),
            "{hint}"
        );
    }

    /// F4: один `code_path` у двух владельцев — второй владелец не теряется,
    /// внешнее ребро снимает обоих (раньше `or_insert_with` оставлял одного, и
    /// второй всплывал ложным «мёртвым»).
    #[test]
    fn common_module_owner_by_path_collision_keeps_both_owners() {
        let conn = mem();
        add_object(&conn, "CommonModule.А", "CommonModule", "А");
        add_object(&conn, "CommonModule.Б", "CommonModule", "Б");
        let shared = "CommonModules/Общий/Ext/Module.bsl";
        add_module(&conn, shared, "CommonModules.А");
        add_module(&conn, shared, "CommonModules.Б");
        add_edge(
            &conn,
            "CommonModules/Внешний/Ext/Module.bsl::П",
            "E",
            Some(&format!("{shared}::E")),
        );
        let scan = scan_repository(&conn, None).unwrap();
        let names = result_names(&scan);
        assert!(
            !names.contains(&"CommonModule.А".to_string())
                && !names.contains(&"CommonModule.Б".to_string()),
            "общий путь снимает ОБОИХ владельцев: {names:?}"
        );
        assert_eq!(scan.channels[9], 2);
    }

    /// E5: пустой граф при непустых модулях — канал не подтверждён
    /// (`common_module_calls_not_indexed`), хотя раньше проходил как «есть».
    #[test]
    fn common_module_channel_unconfirmed_when_graph_empty_but_modules_present() {
        let conn = mem();
        add_object(&conn, "CommonModule.А", "CommonModule", "А");
        add_module(&conn, "CommonModules/А/Ext/Module.bsl", "CommonModules.А");
        // proc_call_graph пуст.
        let scan = scan_repository(&conn, None).unwrap();
        assert!(
            scan.channels_not_applied
                .iter()
                .any(|c| c == "common_module_calls_not_indexed"),
            "пустой граф + непустые модули — канал не подтверждён: {:?}",
            scan.channels_not_applied
        );
        // Сам объект остаётся кандидатом: нулевой счётчик нельзя читать как «нет ссылок».
        assert!(result_names(&scan).contains(&"CommonModule.А".to_string()));
    }

    /// E10: новые строки ловятся единым EXISTS-каналом `code_usages`; счётчики
    /// `common_pictures`/`style_items` ведутся в том же проходе; объект снимается
    /// именно из-за новой ссылки.
    #[test]
    fn new_kind_usages_share_single_code_usages_pass() {
        let conn = mem();
        add_object(&conn, "CommonPicture.Папка", "CommonPicture", "Папка");
        add_object(&conn, "StyleItem.Цвет", "StyleItem", "Цвет");
        add_object(&conn, "StyleItem.Нигде", "StyleItem", "Нигде");
        add_usage(
            &conn,
            "common_picture",
            "CommonPicture.Папка",
            "commonpicture.папка",
            None,
            "Styles/Основной/Ext/Style.xml",
        );
        add_usage(
            &conn,
            "style_item",
            "StyleItem.Цвет",
            "styleitem.цвет",
            None,
            "Styles/Основной/Ext/Style.xml",
        );
        let scan = scan_repository(&conn, None).unwrap();
        assert_eq!(result_names(&scan), vec!["StyleItem.Нигде".to_string()]);
        assert_eq!(scan.channels[2], 2, "универсальный code_usages снял оба");
        assert_eq!(scan.channels[7], 1, "common_pictures");
        assert_eq!(scan.channels[8], 1, "style_items");
    }

    /// E12(e)/E1: 500 общих модулей — сопоставление не деградирует (в новой
    /// реализации нет OR-диапазонов по 400 путей: один проход по графу). Тест
    /// ловит возврат к квадратичной/сканирующей деградации.
    #[test]
    fn common_module_channel_scales_to_many_paths() {
        let conn = mem();
        const N: usize = 500;
        for i in 0..N {
            let name = format!("М{i:03}");
            add_object(
                &conn,
                &format!("CommonModule.{name}"),
                "CommonModule",
                &name,
            );
            add_module(
                &conn,
                &format!("CommonModules/{name}/Ext/Module.bsl"),
                &format!("CommonModules.{name}"),
            );
        }
        // Внешнее ребро только к первому модулю; у остальных — только self-рёбра.
        add_edge(
            &conn,
            "CommonModules/Внешний/Ext/Module.bsl::П",
            "Extern",
            Some("CommonModules/М000/Ext/Module.bsl::Extern"),
        );
        for i in 0..N {
            let p = format!("CommonModules/М{i:03}/Ext/Module.bsl");
            add_edge(
                &conn,
                &format!("{p}::П"),
                "Self",
                Some(&format!("{p}::Self")),
            );
        }
        let t = std::time::Instant::now();
        let scan = scan_repository(&conn, None).unwrap();
        let elapsed = t.elapsed();
        let names = result_names(&scan);
        assert!(
            !names.contains(&"CommonModule.М000".to_string()),
            "внешний вызов снят: {names:?}"
        );
        assert!(
            names.contains(&"CommonModule.М001".to_string()),
            "self-ребро не снимает: {names:?}"
        );
        assert_eq!(scan.channels[9], 1);
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "500 путей не должны деградировать: {elapsed:?}"
        );
    }

    /// E3/F1b/F1c: формы обращения к общим модулям, которые НЕ индексируются,
    /// всегда перечислены в `channels_not_applied` — пропуск канала виден, а не
    /// молчит. Обход списка ловит и переименование форм (G-2: квалифицированная/
    /// метод-форма `ОбщегоНазначения.ОбщийМодуль("X")` и `ОбщийМодуль(Выражение)`
    /// названы одним именем `common_module_via_qualified_or_expression`).
    #[test]
    fn background_job_common_module_form_is_listed() {
        let conn = mem();
        let scan = scan_repository(&conn, None).unwrap();
        for form in NOT_INDEXED_COMMON_MODULE_FORMS {
            assert!(
                scan.channels_not_applied.iter().any(|c| c == form),
                "форма {form} обязана быть в channels_not_applied: {:?}",
                scan.channels_not_applied
            );
        }
        assert!(
            scan.channels_not_applied
                .iter()
                .any(|c| c == "common_module_calls_via_background_job"),
            "форма фонового задания обязана быть в channels_not_applied: {:?}",
            scan.channels_not_applied
        );
        assert!(
            scan.channels_not_applied
                .iter()
                .any(|c| c == "common_module_via_qualified_or_expression"),
            "квалифицированная/метод-форма (и `ОбщийМодуль(Выражение)`) обязана \
             быть названа: {:?}",
            scan.channels_not_applied
        );
        let hint = build_hint(&scan, None);
        assert!(
            hint.contains("common_module_via_qualified_or_expression"),
            "hint обязан называть форму: {hint}"
        );
    }

    /// Проверка МЕХАНИЗМА (не `execute`): долгий запрос, запущенный в
    /// `spawn_blocking`, реально прерывается таймером (`sqlite3_interrupt`), а
    /// не занимает поток целиком — на async-воркере без `spawn_blocking`
    /// задача-таймер не успевала бы сработать. Что этот же механизм реально
    /// включён в `execute` (утяжелённый обход прерывается и tool возвращает
    /// ошибку, а не висит), проверяет интеграционный тест
    /// `find_unreferenced_objects_execute_interrupts_heavy_scan`
    /// (`tests/tools_integration.rs`): сам `execute` здесь недоступен — нужен
    /// `ToolContext` со storage.
    #[tokio::test]
    async fn blocking_query_is_interrupted_by_timer() {
        let conn = Connection::open_in_memory().unwrap();
        let handle = conn.get_interrupt_handle();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            handle.interrupt();
        });
        let t = std::time::Instant::now();
        let res: rusqlite::Result<i64> = tokio::task::spawn_blocking(move || {
            conn.query_row(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 1000000000) \
                 SELECT count(*) FROM c",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
        timer.abort();
        let interrupted = matches!(
            &res,
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::OperationInterrupted
        );
        assert!(interrupted, "запрос обязан быть прерван таймером: {res:?}");
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "прерывание сработало не сразу: {:?}",
            t.elapsed()
        );
    }
}
