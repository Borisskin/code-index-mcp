//! Граф вызовов процедур 1С: сборка слоёв, резолв адресов вызываемых
//! процедур и отсев платформенного балласта.

use anyhow::Result;
use rusqlite::params;
use std::path::Path;

use super::*;

// ───────────────────────── Инкрементальное обновление ─────────────────────
//
// Slice-rebuild графа вызовов и per-object/per-file апдейт XML-слоёв для
// файлов одного watcher-батча. Семантика идентична полному `run_index_extras`
// (см. тест эквивалентности в конце файла). Новых таблиц/колонок не вводит —
// все slice-функции дедуплицированы так же, как полное построение
// (`build_call_graph`), и `find_path_bsl`/`find_data_path` это не затрагивает.

/// Точечно обновить слой `direct` графа вызовов для ОДНОГО файла.
///
/// proc_call_graph дедуплицирован и не помнит источник ребра, поэтому
/// «прежние» рёбра файла берём из side-таблицы `direct_edge_files`, а
/// «текущие» — из core-таблицы `calls` (её базовый индексатор уже обновил
/// по этому файлу к моменту вызова). Трогаем только рёбра этого файла:
///   1) прежние рёбра файла, которых больше нет ни в одном файле
///      (проверка `calls` — она глобальна и актуальна), удаляем из графа;
///   2) текущие рёбра файла доинсертим (существующие отсекает UNIQUE).
///
/// Стоимость — O(рёбер одного файла), не зависит от размера графа.
pub(crate) fn update_call_graph_direct_for_file(
    repo_root: &Path,
    conn: &rusqlite::Connection,
    abs_path: &Path,
) -> Result<()> {
    // rel-путь в формате files.path (forward slash, относительно корня репо).
    let rel = abs_path
        .strip_prefix(repo_root)
        .unwrap_or(abs_path)
        .to_string_lossy()
        .replace('\\', "/");

    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;

    // Прежние рёбра файла (из side-карты).
    let old: Vec<(String, String)> = {
        let mut st = conn.prepare(
            "SELECT caller, callee FROM direct_edge_files \
             WHERE repo = ?1 AND source_file = ?2",
        )?;
        let v = st
            .query_map(params![REPO_DEFAULT, &rel], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
        v
    };
    // Текущие рёбра файла (из calls; для удалённого файла — пусто, files-строки нет).
    let new: Vec<(String, String)> = {
        let mut st = conn.prepare(
            "SELECT DISTINCT c.caller, c.callee \
             FROM calls c JOIN files f ON f.id = c.file_id \
             WHERE f.path = ?1 AND c.caller IS NOT NULL AND c.callee IS NOT NULL",
        )?;
        let v = st
            .query_map(params![&rel], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
        v
    };

    // Обновляем side-карту файла: снести прежние записи, записать текущие.
    conn.execute(
        "DELETE FROM direct_edge_files WHERE repo = ?1 AND source_file = ?2",
        params![REPO_DEFAULT, &rel],
    )?;
    {
        let mut ins = conn.prepare(
            "INSERT OR IGNORE INTO direct_edge_files (repo, caller, callee, source_file) \
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (caller, callee) in &new {
            ins.execute(params![REPO_DEFAULT, caller, callee, &rel])?;
        }
    }

    // Рёбра файла снимаются ЦЕЛИКОМ — и `direct`, и `platform`; затем текущие
    // рёбра файла вставляются заново как `direct`. Ключ вызывателя привязан к
    // файлу (`<rel>::<caller>`), поэтому ребро принадлежит ровно этому файлу и
    // удаляется безусловно: прежняя проверка «файл всё ещё даёт это ребро»
    // (нужная для голых ключей, чтобы не снести ребро другого модуля) при
    // path-привязке не только лишняя, но и неверная — она удержала бы мёртвое
    // ребро файла, если одноимённую пару даёт другой модуль. И, что важнее,
    // она оставляла прежнюю пометку `platform` рядом со свежим `direct`, а
    // классификация переводит `direct` в `platform` и упирается в UNIQUE по
    // (caller, callee, call_type) — точечное обновление надстройки падало.
    {
        let mut del = conn.prepare(
            "DELETE FROM proc_call_graph \
             WHERE repo = ?1 AND caller_proc_key = ?2 AND callee_proc_name = ?3 \
               AND call_type IN ('direct', 'platform')",
        )?;
        for e in &old {
            let caller_key = format!("{}::{}", rel, e.0);
            del.execute(params![REPO_DEFAULT, caller_key, &e.1])?;
        }
    }

    // Текущие рёбра файла → в граф (существующие отсекает UNIQUE без записи).
    // caller_proc_key привязан к файлу: `<rel>::<caller>` (как в build_call_graph).
    {
        let mut ins = conn.prepare(
            "INSERT OR IGNORE INTO proc_call_graph \
             (repo, caller_proc_key, callee_proc_name, call_type) \
             VALUES (?1, ?2, ?3, 'direct')",
        )?;
        for (caller, callee) in &new {
            let caller_key = format!("{}::{}", rel, caller);
            ins.execute(params![REPO_DEFAULT, caller_key, callee])?;
        }
    }

    // callee_proc_key здесь НЕ резолвим: новые рёбра остаются с NULL-адресом,
    // а резолв всего графа (resolve_and_prune_direct_edges) вызывающий делает
    // ОДИН раз после пофайлового цикла батча. Пофайловый резолв раньше
    // (build_common_module_methods + resolve_* + prune_* на КАЖДЫЙ .bsl) давал
    // квадратичную деградацию на bulk-батче: каждый файл заново сканировал весь
    // proc_call_graph и пересобирал tmp-карты общих модулей.
    conn.execute("COMMIT", [])?;
    // Строки на каждый файл здесь нет намеренно: на пачке в тысячи файлов она
    // раздувает журнал и ничего не добавляет — по ней всё равно смотрят итог
    // по пачке, а он печатается после цикла («граф вызовов, пофайловая часть»).
    Ok(())
}

/// Удаление одного не-direct слоя графа (`?2` — `subscription` / `form_event` /
/// `extension_override`). Условие `call_type <> 'direct'` в запросе обязательно:
/// частичный индекс `idx_pcg_call_type_nd` SQLite берёт, только если условие
/// индекса буквально есть в запросе; без него — просмотр всех рёбер графа
/// (замер на 614 тыс. рёбер: 3,3 с на холодной базе вместо 0,014 с).
pub(crate) const DELETE_CALL_LAYER_SQL: &str =
    "DELETE FROM proc_call_graph WHERE repo = ?1 AND call_type = ?2 AND call_type <> 'direct'";

/// Пересобрать слой `subscription` графа вызовов из таблицы
/// `event_subscriptions`. Идентично subscription-части `build_call_graph`.
pub(crate) fn rebuild_call_graph_subscription(conn: &rusqlite::Connection) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(DELETE_CALL_LAYER_SQL, params![REPO_DEFAULT, "subscription"])?;
    let n = conn.execute(
        "INSERT OR IGNORE INTO proc_call_graph \
         (repo, caller_proc_key, callee_proc_name, call_type) \
         SELECT ?, 'event::' || event, handler_module || '.' || handler_proc, 'subscription' \
         FROM event_subscriptions \
         WHERE repo = ? AND handler_module != '' AND handler_proc != ''",
        params![REPO_DEFAULT, REPO_DEFAULT],
    )?;
    resolve_subscription_keys(conn)?;
    conn.execute("COMMIT", [])?;
    tracing::debug!("proc_call_graph subscription (slice-rebuild): {} рёбер", n);
    Ok(())
}

/// Пересобрать слой `form_event` графа вызовов из таблицы `metadata_forms`.
/// Идентично form_event-части `build_call_graph`.
pub(crate) fn rebuild_call_graph_form_event(conn: &rusqlite::Connection) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(DELETE_CALL_LAYER_SQL, params![REPO_DEFAULT, "form_event"])?;
    let rows: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT owner_full_name, form_name, handlers_json \
             FROM metadata_forms WHERE repo = ?",
        )?;
        let mapped = stmt
            .query_map(params![REPO_DEFAULT], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        mapped
    };
    let mut form_count = 0usize;
    {
        let mut insert = conn.prepare(
            "INSERT OR IGNORE INTO proc_call_graph \
             (repo, caller_proc_key, callee_proc_name, call_type) \
             VALUES (?, ?, ?, 'form_event')",
        )?;
        for (owner, form_name, handlers_json) in rows {
            let parsed: Vec<serde_json::Value> =
                serde_json::from_str(&handlers_json).unwrap_or_default();
            for h in parsed {
                let event = h.get("event").and_then(|v| v.as_str()).unwrap_or("");
                let handler = h.get("handler").and_then(|v| v.as_str()).unwrap_or("");
                if event.is_empty() || handler.is_empty() {
                    continue;
                }
                let caller_key = format!("form::{}::{}::{}", owner, form_name, event);
                let callee_name = format!("{}::{}::{}", owner, form_name, handler);
                insert.execute(params![REPO_DEFAULT, caller_key, callee_name])?;
                form_count += 1;
            }
        }
    }
    resolve_form_event_keys(conn)?;
    conn.execute("COMMIT", [])?;
    tracing::debug!(
        "proc_call_graph form_event (slice-rebuild): {} рёбер",
        form_count
    );
    Ok(())
}

/// Слой `extension_override` из `functions.override_*`. Выборка идёт по частичному
/// индексу `idx_functions_override` (условие `override_type IS NOT NULL`), а не
/// просмотром всех функций конфигурации.
pub(crate) const EXTENSION_OVERRIDE_INSERT_SQL: &str = "INSERT OR IGNORE INTO proc_call_graph \
     (repo, caller_proc_key, callee_proc_name, call_type) \
     SELECT ?, f.override_target, f.name, 'extension_override' \
     FROM functions f \
     WHERE f.override_type IS NOT NULL AND f.override_target IS NOT NULL \
       AND f.override_target != '' AND f.name != ''";

/// Построить граф вызовов из заполненных metadata_forms,
/// event_subscriptions и core-таблицы `calls`. Удаляет старые ребра
/// этого репо и вставляет свежие — идемпотентно.
/// Полный пересбор слоя `extension_override` из `functions.override_*`.
/// Идентично subscription-/form_event-частям `build_call_graph`. Вызывается
/// инкрементально при изменении `.bsl` — override-данные живут в `functions`,
/// которую core-индексатор обновляет на правку модуля расширения.
pub(crate) fn rebuild_call_graph_extension_override(conn: &rusqlite::Connection) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        DELETE_CALL_LAYER_SQL,
        params![REPO_DEFAULT, "extension_override"],
    )?;
    conn.execute(EXTENSION_OVERRIDE_INSERT_SQL, params![REPO_DEFAULT])?;
    resolve_extension_override_keys(conn)?;
    conn.execute("COMMIT", [])?;
    Ok(())
}

/// Этап 4f для слоя `form_event`: адрес обработчика — процедура модуля формы.
///
/// Синтетический `callee_proc_name` этого слоя — `<владелец>::<форма>::<обработчик>`,
/// а модуль формы лежит в `metadata_modules` под `object_name =
/// '<владелец>.Form.<форма>'`. Карта строится один раз во временной таблице:
/// поштучный разбор строки на три части в SQL был бы и нечитаем, и дорог.
///
/// Правило «пустой адрес лучше ложного»: если имени в файле соответствует
/// больше одной процедуры, ребро остаётся без адреса.
fn resolve_form_event_keys(conn: &rusqlite::Connection) -> Result<()> {
    let t = std::time::Instant::now();
    conn.execute_batch(
        r#"
        DROP TABLE IF EXISTS tmp_nd_form;
        CREATE TEMP TABLE tmp_nd_form AS
        SELECT callee_name, MIN(callee_key) AS callee_key FROM (
            SELECT substr(mm.object_name, 1, instr(mm.object_name, '.Form.') - 1) || '::' ||
                   substr(mm.object_name, instr(mm.object_name, '.Form.') + 6) || '::' || fn.name AS callee_name,
                   fl.path || '::' || fn.name AS callee_key
            FROM metadata_modules mm
            JOIN files fl ON fl.path = mm.code_path
            JOIN functions fn ON fn.file_id = fl.id
            WHERE mm.module_type = 'FormModule'
              AND instr(mm.object_name, '.Form.') > 0
              AND fn.name IS NOT NULL AND fn.name != ''
        ) GROUP BY callee_name HAVING COUNT(DISTINCT callee_key) = 1;
        CREATE INDEX IF NOT EXISTS tmp_nd_form_idx ON tmp_nd_form(callee_name);
        UPDATE proc_call_graph
        SET callee_proc_key = (
            SELECT k.callee_key FROM tmp_nd_form k
            WHERE k.callee_name = proc_call_graph.callee_proc_name LIMIT 1)
        WHERE call_type = 'form_event' AND callee_proc_key IS NULL;
        DROP TABLE IF EXISTS tmp_nd_form;
        "#,
    )?;
    tracing::debug!("резолв form_event: {} мс", t.elapsed().as_millis());
    Ok(())
}

/// Этап 4f для слоя `subscription`: адрес обработчика — процедура общего модуля.
///
/// Источник адреса — сама строка подписки (`handler_module` + `handler_proc`).
/// Имя модуля приходится приводить: в подписке оно записано как
/// `CommonModule.Имя` (модуль общего назначения) либо, если обработчик лежит
/// в модуле объекта/менеджера, вообще без префикса; в `metadata_modules` общий
/// модуль значится как `CommonModules.Имя`. Поэтому префикс снимается, а не
/// подставляется вслепую: без этого ветка с префиксом не находила модуль.
fn resolve_subscription_keys(conn: &rusqlite::Connection) -> Result<()> {
    let t = std::time::Instant::now();
    conn.execute_batch(
        r#"
        DROP TABLE IF EXISTS tmp_nd_sub;
        CREATE TEMP TABLE tmp_nd_sub AS
        SELECT caller_key, callee_name, MIN(callee_key) AS callee_key FROM (
            SELECT 'event::' || s.event                      AS caller_key,
                   s.handler_module || '.' || s.handler_proc AS callee_name,
                   fl.path || '::' || fn.name                AS callee_key
            FROM event_subscriptions s
            JOIN metadata_modules mm
              ON mm.module_type = 'Module'
             AND mm.object_name = 'CommonModules.' ||
                 CASE WHEN s.handler_module LIKE 'CommonModule.%'
                      THEN substr(s.handler_module, length('CommonModule.') + 1)
                      ELSE s.handler_module END
            JOIN files fl ON fl.path = mm.code_path
            JOIN functions fn ON fn.file_id = fl.id AND fn.name = s.handler_proc
        ) GROUP BY caller_key, callee_name HAVING COUNT(DISTINCT callee_key) = 1;
        CREATE INDEX IF NOT EXISTS tmp_nd_sub_idx ON tmp_nd_sub(caller_key, callee_name);
        UPDATE proc_call_graph
        SET callee_proc_key = (
            SELECT k.callee_key FROM tmp_nd_sub k
            WHERE k.caller_key = proc_call_graph.caller_proc_key
              AND k.callee_name = proc_call_graph.callee_proc_name LIMIT 1)
        WHERE call_type = 'subscription' AND callee_proc_key IS NULL;
        DROP TABLE IF EXISTS tmp_nd_sub;
        "#,
    )?;
    tracing::debug!("резолв subscription: {} мс", t.elapsed().as_millis());
    Ok(())
}

/// Этап 4f для слоя `extension_override`: адрес перехватчика.
///
/// Адрес перехватчика однозначен по построению — ребро и берётся из строки
/// `functions` с непустым `override_type`. Группировка с `HAVING` нужна на
/// случай одноимённых перехватчиков в разных расширениях: там адрес не
/// выводится, и ребро остаётся без него.
fn resolve_extension_override_keys(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        DROP TABLE IF EXISTS tmp_nd_ovr;
        CREATE TEMP TABLE tmp_nd_ovr AS
        SELECT f.name AS callee_name, MIN(fl.path || '::' || f.name) AS callee_key
        FROM functions f
        JOIN files fl ON fl.id = f.file_id
        WHERE f.override_type IS NOT NULL AND f.name IS NOT NULL AND f.name != ''
        GROUP BY f.name HAVING COUNT(DISTINCT fl.path || '::' || f.name) = 1;
        CREATE INDEX IF NOT EXISTS tmp_nd_ovr_idx ON tmp_nd_ovr(callee_name);
        UPDATE proc_call_graph
        SET callee_proc_key = (
            SELECT k.callee_key FROM tmp_nd_ovr k
            WHERE k.callee_name = proc_call_graph.callee_proc_name LIMIT 1)
        WHERE call_type = 'extension_override' AND callee_proc_key IS NULL;
        DROP TABLE IF EXISTS tmp_nd_ovr;
        "#,
    )?;
    Ok(())
}

/// Этап 4f целиком: адресовать все не-direct слои графа. Вызывается полным
/// пересбором; точечный пересбор слоя зовёт только свою часть.
pub(crate) fn resolve_non_direct_callee_keys(conn: &rusqlite::Connection) -> Result<()> {
    let t = std::time::Instant::now();
    resolve_form_event_keys(conn)?;
    resolve_subscription_keys(conn)?;
    resolve_extension_override_keys(conn)?;
    let left: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM proc_call_graph \
             WHERE call_type <> 'direct' AND callee_proc_key IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    tracing::debug!(
        "резолв не-direct слоёв: {} мс, осталось без адреса {}",
        t.elapsed().as_millis(),
        left
    );
    Ok(())
}

/// Вторичные индексы таблиц графа — те же, что заводит расширение схемы.
///
/// На полном пересборе снимаются перед заливкой и поднимаются после неё. Ключи
/// рёбер приходят вперемешку, поэтому вставка миллиона строк при живых индексах
/// правит их деревья в случайных местах — на дешёвом накопителе это самая
/// дорогая работа из возможных. Построение заново идёт одним проходом с
/// сортировкой. Список обязан совпадать со `schema.rs`: расходятся — после
/// пересбора часть индексов не вернётся, и выдача просядет молча.
const GRAPH_INDEX_NAMES: &[&str] = &[
    "idx_pcg_caller",
    "idx_pcg_call_type_nd",
    "idx_pcg_callee_key",
    "idx_def_source",
];

const GRAPH_INDEX_DDL: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_pcg_caller ON proc_call_graph(repo, caller_proc_key);",
    // Частичный — см. комментарий в schema.rs: тип ребра нужен только не-direct
    // слоям, а direct-рёбра (миллионы) в индекс не попадают.
    "CREATE INDEX IF NOT EXISTS idx_pcg_call_type_nd ON proc_call_graph(repo, call_type) WHERE call_type <> 'direct';",
    // Частичный — см. комментарий в schema.rs: полный планировщик брал для `IS NULL`.
    "CREATE INDEX IF NOT EXISTS idx_pcg_callee_key ON proc_call_graph(repo, callee_proc_key) WHERE callee_proc_key IS NOT NULL;",
    "CREATE INDEX IF NOT EXISTS idx_def_source ON direct_edge_files(repo, source_file);",
];

pub(crate) fn build_call_graph(conn: &rusqlite::Connection) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки

    // Снимаем вне транзакции: DDL внутри неё сложил бы страницы удаляемых
    // деревьев в тот же журнал, от роста которого мы и уходим. Обрыв между
    // снятием и подъёмом оставляет таблицу без вторичных индексов — данные
    // целы, а сами индексы вернёт ближайшее открытие базы (расширение схемы
    // создаёт их с `IF NOT EXISTS`).
    for name in GRAPH_INDEX_NAMES {
        conn.execute_batch(&format!("DROP INDEX IF EXISTS {name};"))?;
    }

    conn.execute("BEGIN", [])?;
    let t_step = std::time::Instant::now();
    conn.execute(
        "DELETE FROM proc_call_graph WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    tracing::debug!(
        "удаление прежних рёбер — {} мс",
        t_step.elapsed().as_millis()
    );

    // ── direct: из core::calls ────────────────────────────────────────
    // Таблица `calls` core содержит ребра «caller имя → callee имя»
    // на уровне исходников. Преобразуем в proc_call_graph с типом
    // `direct`. caller_proc_key — стабильный ключ вызывателя в формате
    // `<rel_path>::<caller>` (через JOIN calls ⋈ files): тот же формат,
    // что у procedure_enrichment.proc_key, что даёт джойн граф↔термы и
    // разводит одноимённые процедуры из разных модулей (две
    // `ОбработкаПроведения` больше не схлопываются в одну строку).
    // callee_proc_name остаётся сырым именем; callee_proc_key (адрес
    // цели) заполняет резолвер на этапе 4e.
    // ── direct + direct_edge_files: материализуем calls⋈files ОДИН раз ──
    // Дорогой JOIN+DISTINCT по calls⋈files раньше гонялся дважды (для
    // proc_call_graph и для direct_edge_files) и в паре с построчной вставкой
    // в индексируемую таблицу деградировал сильнее суммы частей. Собираем
    // распарсенное множество рёбер во временную таблицу один раз и наполняем
    // из неё обе таблицы простыми вставками без повторного JOIN/DISTINCT.
    //
    // Замер: попытка слить `tmp_direct_raw` и `tmp_pcg_direct` в один запрос
    // (DISTINCT сразу с `path`/`caller` в составе) вышла на ~3 с ХУЖЕ: DISTINCT
    // по семи колонкам дороже двух проходов по трём. Оставлено как есть.
    let t_step = std::time::Instant::now();
    conn.execute_batch("DROP TABLE IF EXISTS tmp_direct_raw; CREATE TEMP TABLE tmp_direct_raw AS SELECT DISTINCT f.path AS path, c.caller AS caller, c.callee AS callee FROM calls c JOIN files f ON f.id = c.file_id WHERE c.caller IS NOT NULL AND c.callee IS NOT NULL;")?;
    tracing::debug!(
        "выборка рёбер из вызовов — {} мс",
        t_step.elapsed().as_millis()
    );

    // Рёбра слоя `direct` собираются во временной таблице и там же проходят
    // резолв и отсев. Прежде они попадали в основную таблицу сразу, целиком: на
    // типовой торговой конфигурации это 1,17 млн строк, из которых отсев тут же
    // выбрасывал 622 тыс. — больше половины вставок делалось, чтобы через
    // минуту быть удалёнными, и каждая обновляла ключ уникальности и четыре
    // индекса, а её страницы ложились в журнал. Временная таблица живёт в
    // памяти (SQLite собран с `SQLITE_TEMP_STORE=2`), индексов и журнала у неё
    // нет. Набор колонок повторяет основную таблицу, чтобы запросы резолва были
    // дословно теми же — иначе полный пересбор и точечное обновление разошлись
    // бы двумя реализациями одного правила.
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_direct;
         CREATE TEMP TABLE tmp_pcg_direct (
             repo TEXT NOT NULL,
             caller_proc_key TEXT NOT NULL,
             callee_proc_name TEXT NOT NULL,
             callee_proc_key TEXT,
             call_type TEXT NOT NULL
         );",
    )?;
    conn.execute(
        "INSERT INTO tmp_pcg_direct (repo, caller_proc_key, callee_proc_name, call_type) \
         SELECT ?, path || '::' || caller, callee, 'direct' FROM tmp_direct_raw",
        params![REPO_DEFAULT],
    )?;

    // Побочная таблица источников рёбер: строк столько же, сколько рёбер до
    // отсева (1,17 млн на типовой торговой конфигурации), и у неё свой ключ
    // уникальности из четырёх текстовых полей. Порядок вставки задан этим
    // ключом — строки ложатся в его дерево по возрастанию, а не вразнобой.
    let t_step = std::time::Instant::now();
    conn.execute(
        "DELETE FROM direct_edge_files WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    let def_count = conn.execute(
        "INSERT OR IGNORE INTO direct_edge_files (repo, caller, callee, source_file) \
         SELECT ?, caller, callee, path FROM tmp_direct_raw ORDER BY caller, callee, path",
        params![REPO_DEFAULT],
    )?;
    conn.execute_batch("DROP TABLE IF EXISTS tmp_direct_raw;")?;
    tracing::debug!(
        "таблица источников рёбер: {} строк за {} мс",
        def_count,
        t_step.elapsed().as_millis()
    );

    // ── subscription: event_subscriptions → ребро ────────────────────
    // caller_proc_key для подписок — это «виртуальный триггер» вида
    // `<source>::<event>`, например `cfg:DocumentRef.Реализация::ПриЗаписи`.
    // Это не реальная процедура, а событие платформы — но в графе оно
    // занимает позицию вызывателя. callee — `<handler_module>.<handler_proc>`.
    let subscription_count = conn.execute(
        "INSERT OR IGNORE INTO proc_call_graph \
         (repo, caller_proc_key, callee_proc_name, call_type) \
         SELECT \
            ?, \
            'event::' || event, \
            handler_module || '.' || handler_proc, \
            'subscription' \
         FROM event_subscriptions \
         WHERE repo = ? AND handler_module != '' AND handler_proc != ''",
        params![REPO_DEFAULT, REPO_DEFAULT],
    )?;

    // ── form_event: metadata_forms → ребра ───────────────────────────
    // Каждый `(event, handler)` в handlers_json превращается в ребро.
    // Source — `form::<owner_full_name>::<form_name>::<event>`,
    // callee — `<owner_full_name>::<form_name>::<handler>`. Это
    // не классические module.proc — просто стабильные ключи для графа.
    //
    // SQLite до 3.45 не имеет чистого parsed-JSON для array-iteration,
    // поэтому обрабатываем построчно через rusqlite.
    let mut form_count = 0usize;
    let rows: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT owner_full_name, form_name, handlers_json \
             FROM metadata_forms WHERE repo = ?",
        )?;
        let mapped = stmt
            .query_map(params![REPO_DEFAULT], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        mapped
    };

    {
        let mut insert = conn.prepare(
            "INSERT OR IGNORE INTO proc_call_graph \
             (repo, caller_proc_key, callee_proc_name, call_type) \
             VALUES (?, ?, ?, 'form_event')",
        )?;
        for (owner, form_name, handlers_json) in rows {
            let parsed: Vec<serde_json::Value> =
                serde_json::from_str(&handlers_json).unwrap_or_default();
            for h in parsed {
                let event = h.get("event").and_then(|v| v.as_str()).unwrap_or("");
                let handler = h.get("handler").and_then(|v| v.as_str()).unwrap_or("");
                if event.is_empty() || handler.is_empty() {
                    continue;
                }
                let caller_key = format!("form::{}::{}::{}", owner, form_name, event);
                let callee_name = format!("{}::{}::{}", owner, form_name, handler);
                insert.execute(params![REPO_DEFAULT, caller_key, callee_name])?;
                form_count += 1;
            }
        }
    }

    // ── extension_override: перехваты расширений (&Перед/&После/&Вместо) ──
    // Данные уже в functions.override_type/override_target (заполняет парсер
    // bsl::extract_override_info при core-индексации) — отдельный парсер CFE НЕ
    // нужен. Ребро: вызов БАЗОВОГО метода (override_target) достигает
    // реализации-перехватчика (имя функции-перехватчика). По голому имени — как
    // direct-рёбра (общий предел резолва, этап 4e). Так `find_path_bsl` проходит
    // «сквозь &Вместо»: путь до базового метода продолжается в перехватчик.
    let override_count = conn.execute(
        "INSERT OR IGNORE INTO proc_call_graph \
         (repo, caller_proc_key, callee_proc_name, call_type) \
         SELECT ?, f.override_target, f.name, 'extension_override' \
         FROM functions f \
         WHERE f.override_type IS NOT NULL AND f.override_target IS NOT NULL \
           AND f.override_target != '' AND f.name != ''",
        params![REPO_DEFAULT],
    )?;

    // ── этап 4f: адреса не-direct слоёв ──────────────────────────────
    // Три слоя выше собраны голыми именами (как `direct` до 4e); их адреса
    // выводятся из данных, которые уже лежат в базе, — см. комментарии к
    // `resolve_*_keys`. Правило то же: пустой адрес лучше ложного.
    //
    // Место выбрано намеренно ДО вставки direct-рёбер: в таблице сейчас только
    // не-direct слои (десятки тысяч строк), а индексы графа на время полного
    // пересбора сняты — после вставки миллиона direct-рёбер те же три UPDATE
    // шли бы полным сканом по большой таблице.
    resolve_non_direct_callee_keys(conn)?;

    // ── этап 4e + 4e-D + 4e-prune: резолв callee_proc_key + отсев балласта ──
    // Общий с инкрементом хелпер: run_incremental_extras зовёт его же ОДИН раз
    // после пофайловой вставки рёбер батча → идентичность full↔incremental.
    resolve_and_prune_direct_edges(conn, EdgeScope::All, "tmp_pcg_direct")?;

    // Выжившие рёбра — в основную таблицу. Порядок вставки задан ключом
    // уникальности: строки ложатся в его дерево по возрастанию, а не вразнобой,
    // и страниц переписывается меньше.
    let t_step = std::time::Instant::now();
    let direct_count = conn.execute(
        "INSERT OR IGNORE INTO proc_call_graph \
         (repo, caller_proc_key, callee_proc_name, callee_proc_key, call_type) \
         SELECT repo, caller_proc_key, callee_proc_name, callee_proc_key, call_type \
         FROM tmp_pcg_direct ORDER BY caller_proc_key, callee_proc_name",
        [],
    )?;
    conn.execute_batch("DROP TABLE IF EXISTS tmp_pcg_direct;")?;
    tracing::debug!(
        "перенос выживших рёбер в граф: {} строк за {} мс",
        direct_count,
        t_step.elapsed().as_millis()
    );

    let t_step = std::time::Instant::now();
    conn.execute("COMMIT", [])?;
    tracing::debug!("фиксация — {} мс", t_step.elapsed().as_millis());

    // Индексы обратно — уже по готовому набору рёбер, одним проходом на каждый.
    let idx_started = std::time::Instant::now();
    for ddl in GRAPH_INDEX_DDL {
        conn.execute_batch(ddl)?;
    }
    tracing::debug!(
        "индексы графа подняты заново за {} мс",
        idx_started.elapsed().as_millis()
    );

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        (direct_count + subscription_count + form_count + override_count) as u64,
        "ребро",
        "ребра",
        "рёбер",
    ));
    // Число рёбер слоя `direct` — то, что осталось ПОСЛЕ отсева балласта:
    // вставляются теперь только выжившие. Прежде здесь стояло число вставленных
    // до отсева, и итог расходился с содержимым таблицы вдвое.
    tracing::info!(
        "proc_call_graph: {} direct + {} subscription + {} form_event + {} extension_override ребер",
        direct_count,
        subscription_count,
        form_count,
        override_count
    );

    // Этап 4f закрыт: `form_event`, `subscription` и `extension_override`
    // получают адрес цели (`resolve_non_direct_callee_keys`).
    // TODO(этап 4g): external_assignment — runtime-анализ переменных
    // неопределённого типа. Опционально, очень дорогая фича.

    Ok(())
}

/// Этап 4e (общий для полного пересбора и инкремента): заполнить
/// `callee_proc_key` всем direct-рёбрам с NULL-адресом и отсеять
/// платформенный/объектный балласт. Транзакцией управляет вызывающий.
/// Трогает ТОЛЬКО рёбра с `callee_proc_key IS NULL` → идемпотентен: результат
/// идентичен при вызове из `build_call_graph` (после полной вставки рёбер) и из
/// `run_incremental_extras` (после пофайловой вставки рёбер батча).
/// `edges` — имя таблицы рёбер. Полный пересбор ведёт эту работу на временной
/// таблице (она в памяти, без индексов и без журнала) и переносит в основную
/// уже выживший набор; точечное обновление правит основную таблицу на месте.
pub(crate) fn resolve_and_prune_direct_edges(
    conn: &rusqlite::Connection,
    scope: EdgeScope,
    edges: &str,
) -> Result<()> {
    // При полном пересборе справочник экспортных процедур собирается заново:
    // все процедуры конфигурации тут и так читаются. При точечном обновлении
    // он уже актуален — его ведёт пофайловый путь.
    if scope == EdgeScope::All {
        let t = std::time::Instant::now();
        let n = rebuild_exported_procs(conn)?;
        tracing::debug!(
            "справочник экспортных процедур: {} строк за {} мс",
            n,
            t.elapsed().as_millis()
        );
    }
    // Времена шагов — на уровне отладки: по общей строке «граф вызовов» не
    // видно, какой именно шаг сколько занял.
    let t = std::time::Instant::now();
    resolve_direct_callee_keys(conn, scope, edges)?;
    tracing::debug!(
        "резолв: локальные и экспортные адреса — {} мс",
        t.elapsed().as_millis()
    );
    let t = std::time::Instant::now();
    resolve_callee_keys_by_manager(conn, scope, edges)?;
    tracing::debug!(
        "резолв: адреса через менеджеры — {} мс",
        t.elapsed().as_millis()
    );
    let t = std::time::Instant::now();
    resolve_callee_keys_by_form_owner(conn, scope, edges)?;
    tracing::debug!(
        "резолв: вызовы из форм в модуль своего объекта — {} мс",
        t.elapsed().as_millis()
    );
    // Регистронезависимый добор — последним из правил резолва: он берёт только
    // то, что не взяли точные правила, и только при единственном кандидате.
    let t = std::time::Instant::now();
    resolve_case_insensitive_keys(conn, scope, edges)?;
    tracing::debug!(
        "резолв: регистронезависимый добор — {} мс",
        t.elapsed().as_millis()
    );
    // Классификация — ПОСЛЕ резолва (адресуемые рёбра уже адресованы) и ДО
    // отсева: отсев ищет балласт среди direct-рёбер, и платформенные вызовы
    // ему не принадлежат — их место в графе сохраняется, меняется только класс.
    let t = std::time::Instant::now();
    classify_platform_calls(conn, scope, edges)?;
    tracing::debug!(
        "классификация платформенных вызовов — {} мс",
        t.elapsed().as_millis()
    );
    let t = std::time::Instant::now();
    prune_null_direct_edges(conn, scope, edges)?;
    tracing::debug!(
        "отсев: балласт и объектные вызовы одним проходом — {} мс",
        t.elapsed().as_millis()
    );
    Ok(())
}

/// Область резолва: весь граф или только рёбра файлов текущего пакета.
///
/// Рёбра, которые не удалось адресовать однозначно, остаются с пустым адресом
/// навсегда (неоднозначные и платформенные вызовы) — поэтому проход «по всем
/// неадресованным рёбрам» на большой конфигурации каждый раз перебирает одно и
/// то же. При точечном обновлении трогаем только рёбра изменённых файлов.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EdgeScope {
    /// Весь граф репозитория (полный пересбор).
    All,
    /// Только файлы пакета — их пути лежат во временной таблице
    /// `tmp_pcg_scope`, которую готовит [`create_batch_scope`].
    Batch,
}

impl EdgeScope {
    /// Добавка к условию `WHERE`, ограничивающая рёбра файлами пакета.
    ///
    /// Сравнение идёт по равенству целого ключа вызывателя, а не по отрезанию
    /// пути из него: `substr(...)` от индексированного столбца индексом не
    /// пользуется, и просмотр шёл бы по всему графу — ровно то, от чего мы тут
    /// уходим (замер: 6,3 с на пакете из трёх файлов).
    fn clause(self) -> &'static str {
        match self {
            EdgeScope::All => "",
            EdgeScope::Batch => " AND caller_proc_key IN (SELECT key FROM tmp_pcg_keys)",
        }
    }
}

/// Сложить пути файлов пакета во временную таблицу — по ней ограничивается
/// область резолва. Пути — в формате `files.path` (относительные, через `/`).
pub(crate) fn create_batch_scope(conn: &rusqlite::Connection, paths: &[String]) -> Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_scope; CREATE TEMP TABLE tmp_pcg_scope(path TEXT PRIMARY KEY);",
    )?;
    {
        let mut ins = conn.prepare("INSERT OR IGNORE INTO tmp_pcg_scope(path) VALUES (?1)")?;
        for p in paths {
            ins.execute(params![p])?;
        }
    }
    // Ключи вызывателей этих файлов. Берутся из побочной таблицы рёбер, где
    // источник ребра записан явно, — по ней ключ находится сразу, без разбора
    // пути из самого ключа.
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_keys; CREATE TEMP TABLE tmp_pcg_keys(key TEXT PRIMARY KEY);",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO tmp_pcg_keys(key) \
         SELECT DISTINCT source_file || '::' || caller FROM direct_edge_files \
         WHERE repo = ?1 AND source_file IN (SELECT path FROM tmp_pcg_scope)",
        params![REPO_DEFAULT],
    )?;
    Ok(())
}

/// Убрать временные таблицы области.
pub(crate) fn drop_batch_scope(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch("DROP TABLE IF EXISTS tmp_pcg_scope; DROP TABLE IF EXISTS tmp_pcg_keys;")?;
    Ok(())
}

/// Этап 4e: заполнить `callee_proc_key` для direct-рёбер графа — адрес
/// вызываемой процедуры в формате `<rel_path>::<name>` (тот же, что у
/// `caller_proc_key` и `procedure_enrichment.proc_key`). Две безопасные
/// ступени; всё, что статически не выводится однозначно, остаётся NULL
/// (ложная привязка хуже честного NULL).
///
///   (а) **локальный вызов** — голое имя callee объявлено как процедура в том
///       же файле, что и вызыватель (1С: безымянный вызов разрешается в
///       локальный модуль). Адрес = `<файл вызывателя>::<callee>`.
///   (б) **уникальный экспорт** — имя callee принадлежит ровно одной экспортной
///       процедуре во всей конфигурации. Ядро при разборе вызова теряет
///       квалификатор модуля (`Модуль.Метод` → `Метод`), но единственность
///       цели снимает неоднозначность: любой вызов этого имени ведёт именно
///       туда. Экспортность определяется по ключевому слову `Экспорт` после
///       `)` в сигнатуре (поле `functions.args`; отдельного флага нет).
///
/// Неоднозначные (имя экспортно в ≥2 модулях), динамические (`Объект.Метод`
/// по переменной) и платформенные (`Сообщить`, `СтрНайти` — цель вне кода
/// конфигурации) остаются NULL.
pub(crate) fn resolve_direct_callee_keys(
    conn: &rusqlite::Connection,
    scope: EdgeScope,
    edges: &str,
) -> Result<()> {
    // Карта уникальных экспортных имён → путь единственного носителя. Берётся
    // из справочника: он и есть перечень экспортных процедур, читать заново
    // все процедуры конфигурации с текстовым условием больше не нужно.
    let t = std::time::Instant::now();
    // Карта строится только для полного пересбора: на миллионе рёбер она
    // окупается, а пачке нужны десятки имён при справочнике в 261 тыс.
    // процедур — построение карты по всему справочнику стоило бы секунды
    // (замер на пачке одного модуля: 0,93 с на холодной базе; прямой запрос
    // шага (б) — 0,001 с).
    if scope == EdgeScope::All {
        conn.execute_batch(
            "DROP TABLE IF EXISTS tmp_pcg_uexp;
             CREATE TEMP TABLE tmp_pcg_uexp AS
               SELECT name AS nm, MIN(path) AS path FROM exported_procs
               GROUP BY name HAVING COUNT(*) = 1;
             CREATE INDEX tmp_pcg_uexp_idx ON tmp_pcg_uexp(nm);",
        )?;
    }
    tracing::debug!(
        "резолв: карта уникальных экспортов — {} мс",
        t.elapsed().as_millis()
    );

    // (а) локальный вызов: callee объявлен в файле вызывателя.
    //
    // Способ проверки зависит от размера работы, и это подтверждено замером на
    // типовой торговой конфигурации. При полном пересборе рёбер больше миллиона,
    // и дешевле один раз сложить карту «файл + процедура» во временную таблицу
    // (её построение окупается): 4,7 с против 20,5 с у проверки напрямую. При
    // пакете изменений рёбер единицы, и всё наоборот: строить карту по 261 тыс.
    // процедур ради трёх файлов — те самые секунды на ровном месте.
    let t = std::time::Instant::now();
    let local_exists = if scope == EdgeScope::All {
        conn.execute_batch(
            "DROP TABLE IF EXISTS tmp_pcg_funcs;
             CREATE TEMP TABLE tmp_pcg_funcs AS
               SELECT fl.path AS path, fn.name AS nm
               FROM functions fn JOIN files fl ON fl.id = fn.file_id
               WHERE fn.name IS NOT NULL AND fn.name != '';
             CREATE INDEX tmp_pcg_funcs_idx ON tmp_pcg_funcs(path, nm);",
        )?;
        format!(
            "SELECT 1 FROM tmp_pcg_funcs t \
             WHERE t.path = substr({edges}.caller_proc_key, 1, \
                                   instr({edges}.caller_proc_key, '::') - 1) \
               AND t.nm = {edges}.callee_proc_name"
        )
    } else {
        format!(
            "SELECT 1 FROM functions fn JOIN files fl ON fl.id = fn.file_id \
             WHERE fl.path = substr({edges}.caller_proc_key, 1, \
                                    instr({edges}.caller_proc_key, '::') - 1) \
               AND fn.name = {edges}.callee_proc_name"
        )
    };
    tracing::debug!(
        "резолв: карта локальных процедур — {} мс",
        t.elapsed().as_millis()
    );
    let t = std::time::Instant::now();
    conn.execute(
        &format!(
            "UPDATE {edges} \
             SET callee_proc_key = substr(caller_proc_key, 1, instr(caller_proc_key, '::') - 1) \
                                   || '::' || callee_proc_name \
             WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
               AND EXISTS ({}){}",
            local_exists,
            scope.clause()
        ),
        params![REPO_DEFAULT],
    )?;
    tracing::debug!("резолв: локальные адреса — {} мс", t.elapsed().as_millis());

    // (б) уникальный экспорт: имя callee экспортно ровно в одном месте.
    // Полный пересбор идёт по карте tmp_pcg_uexp; пачка спрашивает справочник
    // напрямую по индексу idx_ep_name (repo, name) — ответ тот же, без карты.
    // Условие `ep.repo = ?1` равнозначно прежней карте без фильтра по repo:
    // справочник всегда пишется с REPO_DEFAULT (см. `index_extras/exported.rs`).
    let t = std::time::Instant::now();
    if scope == EdgeScope::All {
        conn.execute(
            &format!(
                "UPDATE {edges} \
                 SET callee_proc_key = ( \
                     SELECT u.path || '::' || u.nm FROM tmp_pcg_uexp u \
                     WHERE u.nm = {edges}.callee_proc_name) \
                 WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
                   AND callee_proc_name IN (SELECT nm FROM tmp_pcg_uexp){}",
                scope.clause()
            ),
            params![REPO_DEFAULT],
        )?;
    } else {
        conn.execute(
            &format!(
                "UPDATE {edges} \
                 SET callee_proc_key = ( \
                     SELECT MIN(ep.path) || '::' || ep.name FROM exported_procs ep \
                     WHERE ep.repo = ?1 AND ep.name = {edges}.callee_proc_name \
                     GROUP BY ep.name HAVING COUNT(*) = 1) \
                 WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
                   AND (SELECT COUNT(*) FROM exported_procs ep \
                        WHERE ep.repo = ?1 AND ep.name = {edges}.callee_proc_name) = 1{}",
                scope.clause()
            ),
            params![REPO_DEFAULT],
        )?;
    }
    tracing::debug!(
        "резолв: уникальные экспорты — {} мс",
        t.elapsed().as_millis()
    );

    // (в) квалифицированный вызов общего модуля: callee хранится склеенным
    // `Модуль.Метод`; по квалификатору точно находим файл общего модуля и его
    // экспортный метод. Заменяет эвристику уникального экспорта для имён,
    // экспортных в ≥2 модулях. Только вызовы с ОДНОЙ точкой (общий модуль);
    // цепочки `Справочники.X.Метод` (менеджеры) — следующий шаг, остаются NULL.
    let t = std::time::Instant::now();
    // Карта методов общих модулей (tmp_pcg_cmeth) строится только полному
    // пересбору; пачка берёт те же данные прямо из справочника — см.
    // resolve_callee_keys_by_qualifier.
    if scope == EdgeScope::All {
        build_common_module_methods(conn)?;
    }
    resolve_callee_keys_by_qualifier(conn, scope, edges)?;
    tracing::debug!(
        "резолв: квалифицированные общие модули — {} мс",
        t.elapsed().as_millis()
    );

    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_funcs; \
         DROP TABLE IF EXISTS tmp_pcg_uexp; \
         DROP TABLE IF EXISTS tmp_pcg_cmeth;",
    )?;
    Ok(())
}

/// Построить temp-таблицу `tmp_pcg_cmeth` экспортных методов общих модулей:
/// `(mname, method, path)`, где `mname` — имя общего модуля (сегмент пути после
/// `CommonModules/`). Используется Tier C резолва (`resolve_callee_keys_by_qualifier`)
/// и в полном пересборе, и в инкременте.
pub(crate) fn build_common_module_methods(conn: &rusqlite::Connection) -> Result<()> {
    // Источник — справочник экспортных процедур: имя общего модуля и вид
    // модуля в нём уже разобраны, читать пути и сигнатуры заново незачем.
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_cmeth;\n\
         CREATE TEMP TABLE tmp_pcg_cmeth AS\n\
           SELECT owner AS mname, name AS method, path AS path\n\
           FROM exported_procs WHERE kind = 'common' AND owner IS NOT NULL;\n\
         CREATE INDEX tmp_pcg_cmeth_idx ON tmp_pcg_cmeth(mname, method);",
    )?;
    Ok(())
}

/// Tier C: резолв `callee_proc_key` по квалификатору общего модуля. callee
/// хранится склеенным `Модуль.Метод`; берём часть до точки как имя модуля,
/// после — как метод, и точно адресуем в файл общего модуля. При `scope = All`
/// источник — заранее построенная `tmp_pcg_cmeth`; при `scope = Batch` — прямо
/// из справочника `exported_procs`. Работает только для вызовов с ОДНОЙ точкой
/// (общий модуль); цепочки `Справочники.X.Метод` пропускаются (остаются NULL).
/// `scope` = `Batch` ограничивает рёбрами файлов пакета (точечное обновление).
pub(crate) fn resolve_callee_keys_by_qualifier(
    conn: &rusqlite::Connection,
    scope: EdgeScope,
    edges: &str,
) -> Result<()> {
    // Замер: предварительная карта «имя → адрес» (JOIN + DISTINCT) выигрыша не
    // дала — построение карты стоит столько же, сколько коррелированный
    // подзапрос, — поэтому оставлен прямой UPDATE с EXISTS.
    //
    // Полный пересбор читает источник из tmp_pcg_cmeth (её строит
    // build_common_module_methods). Пачке карта не нужна: справочник
    // exported_procs отвечает тем же индексом idx_ep_owner (repo, kind, owner,
    // name) напрямую — иначе карта по всему справочнику строилась бы ради
    // десятков имён пачки (замер на пачке одного модуля: 0,93 с на холодной
    // базе против 0,001 с с прямым запросом).
    let mut sql = if scope == EdgeScope::All {
        format!(
            "UPDATE {edges} \
             SET callee_proc_key = ( \
                 SELECT MIN(cm.path || '::' || cm.method) FROM tmp_pcg_cmeth cm \
                 WHERE cm.mname = substr({edges}.callee_proc_name, 1, instr({edges}.callee_proc_name,'.')-1) \
                   AND cm.method = substr({edges}.callee_proc_name, instr({edges}.callee_proc_name,'.')+1)) \
             WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
               AND instr(callee_proc_name,'.') > 0 \
               AND instr(substr(callee_proc_name, instr(callee_proc_name,'.')+1), '.') = 0 \
               AND EXISTS ( \
                 SELECT 1 FROM tmp_pcg_cmeth cm \
                 WHERE cm.mname = substr({edges}.callee_proc_name, 1, instr({edges}.callee_proc_name,'.')-1) \
                   AND cm.method = substr({edges}.callee_proc_name, instr({edges}.callee_proc_name,'.')+1))"
        )
    } else {
        format!(
            "UPDATE {edges} \
             SET callee_proc_key = ( \
                 SELECT MIN(ep.path || '::' || ep.name) FROM exported_procs ep \
                 WHERE ep.repo = ?1 AND ep.kind = 'common' \
                   AND ep.owner = substr({edges}.callee_proc_name, 1, instr({edges}.callee_proc_name,'.')-1) \
                   AND ep.name = substr({edges}.callee_proc_name, instr({edges}.callee_proc_name,'.')+1)) \
             WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
               AND instr(callee_proc_name,'.') > 0 \
               AND instr(substr(callee_proc_name, instr(callee_proc_name,'.')+1), '.') = 0 \
               AND EXISTS ( \
                 SELECT 1 FROM exported_procs ep \
                 WHERE ep.repo = ?1 AND ep.kind = 'common' \
                   AND ep.owner = substr({edges}.callee_proc_name, 1, instr({edges}.callee_proc_name,'.')-1) \
                   AND ep.name = substr({edges}.callee_proc_name, instr({edges}.callee_proc_name,'.')+1))"
        )
    };
    sql.push_str(scope.clause());
    conn.execute(&sql, params![REPO_DEFAULT])?;
    Ok(())
}

/// Имена-«балласт»: методы коллекций/объектов/запросов/выборок и глобальные
/// функции платформы, чья цель лежит ВНЕ кода конфигурации. Ядро стирает
/// приёмник вызова (`Коллекция.Добавить` → `Добавить`), поэтому такие рёбра
/// ведут «в никуда» (callee_proc_key не резолвится) и составляют ~⅓ графа.
/// Список курируемый и намеренно консервативный: имена методов БСП/общих
/// модулей (`ЗначениеРеквизитаОбъекта`, `ПодсистемаСуществует`,
/// `СообщитьПользователю`, `КодОсновногоЯзыка`…) сюда НЕ входят — они резолвятся
/// в реальные процедуры. Дополнительная страховка от коллизий имён — в
/// `prune_platform_balast` удаляются только рёбра с `callee_proc_key IS NULL`.
pub(crate) const PLATFORM_BALAST: &[&str] = &[
    // методы коллекций / объектов / запросов / выборок (приёмник стёрт ядром)
    "Вставить",
    "Добавить",
    "Количество",
    "Найти",
    "Выбрать",
    "Следующий",
    "Получить",
    "Выгрузить",
    "ВыгрузитьКолонку",
    "Записать",
    "НайтиСтроки",
    "Очистить",
    "Удалить",
    "Закрыть",
    "ПолучитьОбъект",
    "Прочитать",
    "Установить",
    "ПолучитьЭлементы",
    "НайтиПоИдентификатору",
    "Свойство",
    "Метаданные",
    "ПолноеИмя",
    "УникальныйИдентификатор",
    "ПустаяСсылка",
    "СоздатьНаборЗаписей",
    // глобальные функции / процедуры платформы
    "ЗначениеЗаполнено",
    "НСтр",
    "Тип",
    "ТипЗнч",
    "Выполнить",
    "СтрЗаменить",
    "СтрШаблон",
    "ПодставитьПараметрыВСтроку",
    "Строка",
    "СокрЛП",
    "СтрСоединить",
    "СтрНайти",
    "СтрДлина",
    "Лев",
    "Сред",
    "Прав",
    "Формат",
    "ТекущаяДатаСеанса",
    "ПредопределенноеЗначение",
    "ОткрытьФорму",
    "Сообщить",
    "УстановитьПривилегированныйРежим",
    "ПолучитьФункциональнуюОпцию",
    "ЗаписьЖурналаРегистрации",
    "НачатьТранзакцию",
    "ЗафиксироватьТранзакцию",
    "ОтменитьТранзакцию",
    "ОчиститьСообщения",
    "ИнформацияОбОшибке",
    "ПодробноеПредставлениеОшибки",
    "ПоместитьВоВременноеХранилище",
    "ПолучитьИзВременногоХранилища",
    "ВыполнитьОбработкуОповещения",
    "ОбщийМодуль",
    "ЗаполнитьЗначенияСвойств",
    "УстановитьПараметр",
    "ОписаниеОповещения",
    "ОписаниеТипов",
    "ПустаяСтрока",
    // конструкторы типов (Новый X — ядро пишет callee = имя типа)
    "Структура",
    "Массив",
    "Запрос",
    "Соответствие",
    "ТаблицаЗначений",
    "СписокЗначений",
];

/// Регистронезависимый резолв адресов — для BSL, где регистр имени не значим.
///
/// Точные правила резолвера сравнивают имена как строки, и этого мало: в 1С
/// `Провести` и `провести` — одно и то же имя, а для базы это две разные строки.
/// Свернуть регистр в SQL нельзя: `lower()` в SQLite не трогает кириллицу
/// (проверено), поэтому карта «имя в нижнем регистре → адрес» собирается в Rust.
///
/// Правило «пустой адрес лучше ложного» соблюдается дважды: имя берётся только
/// из числа уже неразрешённых, и адрес ставится, только если нижнему регистру
/// имени соответствует РОВНО одно определение. Два модуля с процедурами,
/// совпадающими с точностью до регистра, — уже неоднозначность, адрес не ставится.
fn resolve_case_insensitive_keys(
    conn: &rusqlite::Connection,
    scope: EdgeScope,
    edges: &str,
) -> Result<()> {
    let names: Vec<String> = {
        // Область резолва: на точечном проходе — только рёбра файлов пакета.
        // Иначе каждый батч перебирал бы все неадресованные рёбра графа.
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT callee_proc_name FROM {edges} \
             WHERE call_type = 'direct' AND callee_proc_key IS NULL{}",
            scope.clause()
        ))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if names.is_empty() {
        return Ok(());
    }
    let wanted: std::collections::HashSet<String> =
        names.iter().map(|n| n.to_lowercase()).collect();

    // Адрес по имени в нижнем регистре; `None` — имя неоднозначно.
    let mut map: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT fn.name, fl.path FROM functions fn JOIN files fl ON fl.id = fn.file_id \
             WHERE fn.name IS NOT NULL AND fn.name != ''",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let lower = name.to_lowercase();
            if !wanted.contains(&lower) {
                continue;
            }
            let path: String = row.get(1)?;
            let key = format!("{path}::{name}");
            match map.get_mut(&lower) {
                Some(slot) => {
                    if slot.as_deref() != Some(key.as_str()) {
                        *slot = None;
                    }
                }
                None => {
                    map.insert(lower, Some(key));
                }
            }
        }
    }
    let pairs: Vec<(String, String)> = names
        .iter()
        .filter_map(|n| match map.get(&n.to_lowercase()) {
            Some(Some(key)) => Some((n.clone(), key.clone())),
            _ => None,
        })
        .collect();
    if pairs.is_empty() {
        return Ok(());
    }

    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_ci_pairs;
         CREATE TEMP TABLE tmp_ci_pairs(callee TEXT PRIMARY KEY, key TEXT NOT NULL);",
    )?;
    {
        let mut ins =
            conn.prepare("INSERT OR IGNORE INTO tmp_ci_pairs(callee, key) VALUES (?1, ?2)")?;
        for (callee, key) in &pairs {
            ins.execute(params![callee, key])?;
        }
    }
    let n = conn.execute(
        &format!(
            "UPDATE {edges} SET callee_proc_key = \
                 (SELECT key FROM tmp_ci_pairs p WHERE p.callee = {edges}.callee_proc_name) \
             WHERE call_type = 'direct' AND callee_proc_key IS NULL \
               AND callee_proc_name IN (SELECT callee FROM tmp_ci_pairs){}",
            scope.clause()
        ),
        [],
    )?;
    conn.execute_batch("DROP TABLE IF EXISTS tmp_ci_pairs;")?;
    if n > 0 {
        tracing::debug!(
            "регистронезависимый резолв: адресовано {} рёбер по {} именам",
            n,
            pairs.len()
        );
    }
    Ok(())
}

/// Отдельный класс для вызовов глобальных функций платформы.
///
/// Такие рёбра не «неразрешённые»: адреса в конфигурации у них нет по природе.
/// На живой конфигурации это **80 % всех рёбер с пустым адресом** (`Число`,
/// `Дата`, `ПоказатьВопрос`, `НРег`…), поэтому в статистике связывания они
/// искажают картину сильнее всего.
///
/// Отдельный `call_type` нужен не только статистике: канал «прощение по имени»
/// в анализе неиспользуемого кода считает процедуру живой, если её имя совпало с
/// именем неразрешённого ребра. Совпадает оно чаще всего именно с платформенной
/// функцией — то есть сегодня платформенный вызов «спасает» одноимённую
/// процедуру пользователя.
///
/// Безопасность: имя уходит в платформенные ТОЛЬКО если в этой конфигурации у
/// него нет ни одного определения. Иначе пользовательская `Функция Число(...)`
/// потеряла бы входящие вызовы. Сравнение — по нижнему регистру и в Rust:
/// `SQLite` не сворачивает кириллицу функцией `lower()`.
fn classify_platform_calls(
    conn: &rusqlite::Connection,
    scope: EdgeScope,
    edges: &str,
) -> Result<()> {
    // Область резолва та же, что у остальных проходов: на точечном обновлении —
    // только рёбра файлов пакета.
    let mut stmt = conn.prepare(&format!(
        "SELECT DISTINCT callee_proc_name FROM {edges} \
         WHERE call_type = 'direct' AND callee_proc_key IS NULL{}",
        scope.clause()
    ))?;
    let candidates: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let platform: std::collections::HashSet<String> =
        crate::platform_globals::platform_globals_lower()
            .into_iter()
            .collect();
    let matched: Vec<&String> = candidates
        .iter()
        .filter(|n| platform.contains(&n.to_lowercase()))
        .collect();
    if matched.is_empty() {
        return Ok(());
    }

    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pf_names;
         CREATE TEMP TABLE tmp_pf_names(name TEXT PRIMARY KEY);",
    )?;
    {
        let mut ins = conn.prepare("INSERT OR IGNORE INTO tmp_pf_names(name) VALUES (?1)")?;
        for n in &matched {
            ins.execute(params![n])?;
        }
    }
    // Страховка от прежней пометки. Если у ребра уже есть строка `platform`
    // (осталась от прошлого прохода, пока файл ещё не переиндексировали), то
    // UPDATE ниже упрётся в UNIQUE по (caller, callee, call_type) — именно так
    // падало точечное обновление надстройки. Ребро одно и то же, две строки о
    // нём противоречат друг другу: снимаем пометку, UPDATE поставит её заново.
    conn.execute(
        &format!(
            "DELETE FROM {edges} WHERE call_type = 'platform' \
               AND callee_proc_name IN (SELECT name FROM tmp_pf_names) \
               AND (caller_proc_key, callee_proc_name) IN ( \
                   SELECT caller_proc_key, callee_proc_name FROM {edges} \
                   WHERE call_type = 'direct' AND callee_proc_key IS NULL{})",
            scope.clause()
        ),
        [],
    )?;
    let n = conn.execute(
        &format!(
            "UPDATE {edges} SET call_type = 'platform' \
             WHERE call_type = 'direct' AND callee_proc_key IS NULL \
               AND callee_proc_name IN (SELECT name FROM tmp_pf_names) \
               AND NOT EXISTS (SELECT 1 FROM functions f \
                               WHERE f.name = {edges}.callee_proc_name){}",
            scope.clause()
        ),
        [],
    )?;
    conn.execute_batch("DROP TABLE IF EXISTS tmp_pf_names;")?;
    if n > 0 {
        tracing::debug!(
            "платформенные вызовы выделены в отдельный класс: {} рёбер из {} имён",
            n,
            matched.len()
        );
    }
    Ok(())
}

/// Один проход отсева NULL-рёбер `direct`: платформенный балласт + объектные
/// вызовы.
///
/// Раньше это были два отдельных DELETE по одной и той же временной таблице —
/// два полных прохода по 2,3 млн строк. Условия независимы и все удаляют только
/// `callee_proc_key IS NULL`, поэтому объединены в один DELETE с `OR`.
///
/// Защиты от потери реальных рёбер (те же, что были у половин):
///   * удаляются только рёбра с `callee_proc_key IS NULL` — резолвленные
///     в реальную процедуру сохраняются;
///   * имя, экспортное где-либо в конфигурации, не трогается вовсе:
///     подмножество экспортных имён из списка балласта считается ОДНИМ
///     запросом (построчный `NOT IN (SELECT name FROM exported_procs)` на
///     2,3 млн рёбер стоил секунды);
///   * одноточечные и многоточечные объектные вызовы щадят имена общих модулей
///     (их резолвит Tier C) и коллекции метаданных (Tier D).
pub(crate) fn prune_null_direct_edges(
    conn: &rusqlite::Connection,
    scope: EdgeScope,
    edges: &str,
) -> Result<()> {
    // 1. Эффективный список балласта: PLATFORM_BALAST минус экспортные-в-конфиге.
    let balast_all = PLATFORM_BALAST
        .iter()
        .map(|n| format!("'{}'", n))
        .collect::<Vec<_>>()
        .join(",");
    let mut st = conn.prepare(&format!(
        "SELECT DISTINCT name FROM exported_procs WHERE repo = ?1 AND name IN ({balast_all})"
    ))?;
    let exported_in_balast: std::collections::HashSet<String> = st
        .query_map(params![REPO_DEFAULT], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    let effective: Vec<&str> = PLATFORM_BALAST
        .iter()
        .copied()
        .filter(|n| !exported_in_balast.contains(*n))
        .collect();

    // 2. Защиты объектных условий: имена общих модулей и коллекции метаданных.
    // tmp_pmods — имена общих модулей (сегмент пути после CommonModules/).
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pmods;\n\
         CREATE TEMP TABLE tmp_pmods AS\n\
           SELECT DISTINCT substr(seg,1,instr(seg,'/')-1) AS q FROM (\n\
             SELECT substr(path, instr(path,'CommonModules/')+length('CommonModules/')) AS seg\n\
             FROM files WHERE path LIKE '%CommonModules/%/Module.bsl') WHERE instr(seg,'/')>0;\n\
         CREATE INDEX tmp_pmods_idx ON tmp_pmods(q);",
    )?;
    // tmp_pcolls — коллекции метаданных (защита одноточечных менеджер-вызовов).
    conn.execute_batch("DROP TABLE IF EXISTS tmp_pcolls; CREATE TEMP TABLE tmp_pcolls(q TEXT);")?;
    {
        let mut ins = conn.prepare("INSERT INTO tmp_pcolls(q) VALUES (?1)")?;
        for c in METADATA_COLLECTIONS {
            ins.execute(params![c])?;
        }
    }
    conn.execute_batch("CREATE INDEX tmp_pcolls_idx ON tmp_pcolls(q);")?;

    // 3. Один DELETE: балласт ИЛИ одноточечный объектный ИЛИ многоточечный.
    // Имя метода для сопоставления с балластом: callee хранится склеенным
    // (`Объект.Записать`), поэтому берём часть ПОСЛЕ точки; у голых имён — имя
    // целиком.
    let meth = "substr(callee_proc_name, CASE WHEN instr(callee_proc_name,'.')>0 \
                THEN instr(callee_proc_name,'.')+1 ELSE 1 END)";
    let first = "substr(callee_proc_name, 1, instr(callee_proc_name,'.')-1)";
    let single_dot = "instr(substr(callee_proc_name, instr(callee_proc_name,'.')+1), '.') = 0";
    let mut ors: Vec<String> = Vec::new();
    if !effective.is_empty() {
        let list = effective
            .iter()
            .map(|n| format!("'{}'", n))
            .collect::<Vec<_>>()
            .join(",");
        ors.push(format!("{meth} IN ({list})"));
    }
    // Одноточечные `Объект.Метод`: первый сегмент не общий модуль и не коллекция.
    ors.push(format!(
        "(instr(callee_proc_name,'.') > 0 AND {single_dot} \
          AND {first} NOT IN (SELECT q FROM tmp_pmods) \
          AND {first} NOT IN (SELECT q FROM tmp_pcolls))"
    ));
    // Многоточечные `X.Y.Метод`: первый сегмент не общий модуль (объектная
    // цепочка или платформенный метод менеджера, не найденный Tier D).
    ors.push(format!(
        "(instr(substr(callee_proc_name, instr(callee_proc_name,'.')+1), '.') > 0 \
          AND {first} NOT IN (SELECT q FROM tmp_pmods))"
    ));
    let mut sql = format!(
        "DELETE FROM {edges} \
         WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
           AND ({})",
        ors.join(" OR ")
    );
    sql.push_str(scope.clause());
    conn.execute(&sql, params![REPO_DEFAULT])?;
    conn.execute_batch("DROP TABLE IF EXISTS tmp_pmods; DROP TABLE IF EXISTS tmp_pcolls;")?;
    Ok(())
}

/// Коллекции метаданных 1С — менеджеры, доступные как `Справочники.X`,
/// `Документы.X` и т.п. Одноточечный вызов с таким префиксом — обращение к
/// менеджеру (вызов менеджер-модуля), НЕ метод локального объекта. Прун
/// объектных вызовов их щадит: резолв менеджер-модулей — отдельный шаг.
pub(crate) const METADATA_COLLECTIONS: &[&str] = &[
    "Справочники",
    "Документы",
    "ЖурналыДокументов",
    "Перечисления",
    "Отчеты",
    "Обработки",
    "ПланыВидовХарактеристик",
    "ПланыСчетов",
    "ПланыВидовРасчета",
    "РегистрыСведений",
    "РегистрыНакопления",
    "РегистрыБухгалтерии",
    "РегистрыРасчета",
    "БизнесПроцессы",
    "Задачи",
    "ПланыОбмена",
    "Константы",
    "Последовательности",
    "КритерииОтбора",
    "ОпределяемыеТипы",
    // англоязычные эквиваленты (EN-конфигурации)
    "Catalogs",
    "Documents",
    "DocumentJournals",
    "Enums",
    "Reports",
    "DataProcessors",
    "ChartsOfCharacteristicTypes",
    "ChartsOfAccounts",
    "ChartsOfCalculationTypes",
    "InformationRegisters",
    "AccumulationRegisters",
    "AccountingRegisters",
    "CalculationRegisters",
    "BusinessProcesses",
    "Tasks",
    "ExchangePlans",
    "Constants",
    "Sequences",
];

/// Построить temp-таблицу `tmp_pcg_mmeth` экспортных методов менеджер-модулей:
/// `(folder, object, method, path)`. folder/object извлекаем из пути
/// `<...>/<Folder>/<Object>/[Ext/]ManagerModule.bsl` в Rust (в SQLite нет «последнего
/// вхождения» для надёжного разбора двух хвостовых сегментов).
pub(crate) fn build_manager_module_methods(conn: &rusqlite::Connection) -> Result<()> {
    // Источник — справочник экспортных процедур: папка типа и имя объекта в
    // нём уже разобраны из пути (обе раскладки — Конфигуратора и 1C:EDT).
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_mmeth;\n\
         CREATE TEMP TABLE tmp_pcg_mmeth AS\n\
           SELECT folder AS folder, owner AS object, name AS method, path AS path\n\
           FROM exported_procs\n\
           WHERE kind = 'manager' AND owner IS NOT NULL AND folder IS NOT NULL;\n\
         CREATE INDEX tmp_pcg_mmeth_idx ON tmp_pcg_mmeth(folder, object, method);",
    )?;
    Ok(())
}

/// Построить temp-таблицу `tmp_pcg_coll` (форма-обращения → папка метаданных) из
/// единой таблицы META_FORMS (`code_usages`). RU и EN формы ведут в одну папку.
pub(crate) fn build_collection_folder_map(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_coll; CREATE TEMP TABLE tmp_pcg_coll(coll TEXT, folder TEXT);",
    )?;
    {
        let mut ins = conn.prepare("INSERT INTO tmp_pcg_coll(coll, folder) VALUES (?1,?2)")?;
        for (coll, folder) in crate::code_usages::collection_folder_pairs() {
            ins.execute(params![coll, folder])?;
        }
    }
    conn.execute_batch("CREATE INDEX tmp_pcg_coll_idx ON tmp_pcg_coll(coll);")?;
    Ok(())
}

/// Tier D: резолв менеджер-вызовов `Коллекция.Объект.Метод` (ровно 2 точки).
/// Коллекцию маппим в папку метаданных, ищем экспортный метод в
/// `<Папка>/<Объект>/[Ext/]ManagerModule.bsl`. Платформенные методы менеджера
/// (`ПустаяСсылка`, `НайтиПоКоду`) не экспортны в модуле → остаются NULL.
/// `scope` = `Batch` — в области файлов пакета.
pub(crate) fn resolve_callee_keys_by_manager(
    conn: &rusqlite::Connection,
    scope: EdgeScope,
    edges: &str,
) -> Result<()> {
    build_manager_module_methods(conn)?;
    build_collection_folder_map(conn)?;
    let col = format!("{edges}.callee_proc_name");
    let s1 = format!("substr({col},1,instr({col},'.')-1)");
    let rest = format!("substr({col},instr({col},'.')+1)");
    let s2 = format!("substr({rest},1,instr({rest},'.')-1)");
    let s3 = format!("substr({rest},instr({rest},'.')+1)");
    let twodots = format!("(length({col})-length(replace({col},'.','')))=2");
    let join_cond = format!("cc.coll = {s1} AND mm.object = {s2} AND mm.method = {s3}");
    let mut sql = format!(
        "UPDATE {edges} \
         SET callee_proc_key = ( \
             SELECT MIN(mm.path || '::' || mm.method) \
             FROM tmp_pcg_coll cc JOIN tmp_pcg_mmeth mm ON mm.folder = cc.folder \
             WHERE {join_cond}) \
         WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL AND {twodots} \
           AND EXISTS ( \
             SELECT 1 FROM tmp_pcg_coll cc JOIN tmp_pcg_mmeth mm ON mm.folder = cc.folder \
             WHERE {join_cond})"
    );
    sql.push_str(scope.clause());
    conn.execute(&sql, params![REPO_DEFAULT])?;
    conn.execute_batch("DROP TABLE IF EXISTS tmp_pcg_mmeth; DROP TABLE IF EXISTS tmp_pcg_coll;")?;
    Ok(())
}

/// Кандидаты в модуль объекта, которому принадлежит модуль формы `path`.
/// Пустой вектор — `path` не модуль формы объекта.
///
/// Раскладка Конфигуратора — `<X>/Forms/<Форма>/Ext/Form/Module.bsl` → модуль
/// объекта `<X>/Ext/ObjectModule.bsl`; 1C:EDT — `<X>/Forms/<Форма>/Module.bsl`
/// → `<X>/ObjectModule.bsl`; внешняя обработка/отчёт —
/// `<X>/Form/<Форма>/Form.obj.bsl` → `<X>/ExternalDataProcessor.obj.bsl`, затем
/// `<X>/ExternalReport.obj.bsl`. Имя формы — ровно один сегмент без `/`.
/// Раскладка Конфигуратора проверяется раньше EDT: её хвост тоже оканчивается
/// на `/Module.bsl`, но у EDT между `/Forms/<Форма>/` и `Module.bsl` ничего нет.
/// Всё прочее (в том числе `.../CommonForms/<Ф>/Ext/Form/Module.bsl` — там нет
/// сегмента `/Forms/`) → пустой вектор.
pub(crate) fn form_owner_object_module_candidates(path: &str) -> Vec<String> {
    if let Some(idx) = path.rfind("/Forms/") {
        let head = &path[..idx];
        let form = &path[idx + "/Forms/".len()..];
        if !head.is_empty() {
            if let Some(name) = form.strip_suffix("/Ext/Form/Module.bsl") {
                if !name.is_empty() && !name.contains('/') {
                    return vec![format!("{head}/Ext/ObjectModule.bsl")];
                }
            }
            if let Some(name) = form.strip_suffix("/Module.bsl") {
                if !name.is_empty() && !name.contains('/') {
                    return vec![format!("{head}/ObjectModule.bsl")];
                }
            }
        }
    }
    if let Some(idx) = path.rfind("/Form/") {
        let head = &path[..idx];
        let form = &path[idx + "/Form/".len()..];
        if !head.is_empty() {
            if let Some(name) = form.strip_suffix("/Form.obj.bsl") {
                if !name.is_empty() && !name.contains('/') {
                    return vec![
                        format!("{head}/ExternalDataProcessor.obj.bsl"),
                        format!("{head}/ExternalReport.obj.bsl"),
                    ];
                }
            }
        }
    }
    Vec::new()
}

/// Правило (д): вызов из модуля формы → модуль объекта этой формы.
///
/// В 1С из формы нельзя вызвать процедуру модуля объекта иначе как через
/// переменную с объектом (`РеквизитФормыВЗначение("Объект")`, `ЭтотОбъект()`),
/// поэтому одноточечный вызов из модуля формы, чьё имя метода экспортно в
/// модуле объекта этой формы, привязывается туда. Замер на типовой торговой
/// конфигурации: 501 такой вызов, 91 % подтверждён по присваиванию переменной.
/// Не привязываются: вызовы из процедур с директивой `&НаКлиенте` и вызовы через
/// переменную `Модуль<Имя>`, если общий модуль `<Имя>` существует. Замер на
/// бухгалтерии (выгрузки EDT и Конфигуратора): из 2830 связей правила оба
/// исключения убирают 60 ложных, верных — ни одной.
/// Правило идёт после (в) и (г), до отсева. Транзакцией не управляет.
pub(crate) fn resolve_callee_keys_by_form_owner(
    conn: &rusqlite::Connection,
    scope: EdgeScope,
    edges: &str,
) -> Result<()> {
    // Пары «модуль формы → модуль объекта» области резолва. Пути читаем в Vec
    // до вставок: открытый SELECT во время INSERT'ов держать не нужно.
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_formobj;
         CREATE TEMP TABLE tmp_pcg_formobj(form_path TEXT PRIMARY KEY, obj_path TEXT NOT NULL);",
    )?;
    let form_paths: Vec<String> = {
        let sql = match scope {
            EdgeScope::All => {
                "SELECT path FROM files WHERE path LIKE '%/Forms/%' OR path LIKE '%/Form/%'"
            }
            EdgeScope::Batch => "SELECT path FROM tmp_pcg_scope",
        };
        let mut st = conn.prepare(sql)?;
        let v = st
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        v
    };
    {
        let mut known =
            conn.prepare("SELECT 1 FROM exported_procs WHERE repo = ?1 AND path = ?2 LIMIT 1")?;
        let mut ins = conn.prepare(
            "INSERT OR IGNORE INTO tmp_pcg_formobj(form_path, obj_path) VALUES (?1, ?2)",
        )?;
        for path in &form_paths {
            // Первый кандидат, для которого модуль объекта реально есть в
            // справочнике (у внешних обработки/отчёта кандидатов два).
            let obj = form_owner_object_module_candidates(path)
                .into_iter()
                .find(|c| {
                    known
                        .query_row(params![REPO_DEFAULT, c], |_| Ok(()))
                        .is_ok()
                });
            if let Some(obj) = obj {
                ins.execute(params![path, obj])?;
            }
        }
    }
    let found: i64 = conn.query_row("SELECT COUNT(*) FROM tmp_pcg_formobj", [], |r| r.get(0))?;
    if found == 0 {
        conn.execute_batch("DROP TABLE IF EXISTS tmp_pcg_formobj;")?;
        return Ok(());
    }
    // Клиентские процедуры форм области. Из процедуры `&НаКлиенте` модуль объекта
    // недоступен — в переменной там клиентская форма или общий модуль, привязывать
    // такой вызов к модулю объекта нельзя. Директиву разборщик кладёт в
    // `functions.docstring` («procedure &НаКлиенте export»); пробелы вокруг образца
    // отсекают `&НаКлиентеНаСервереБезКонтекста`. Собираем только по формам из
    // tmp_pcg_formobj: на точечном обновлении это формы пакета.
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_formclient;
         CREATE TEMP TABLE tmp_pcg_formclient(k TEXT PRIMARY KEY);
         INSERT OR IGNORE INTO tmp_pcg_formclient(k)
           SELECT m.form_path || '::' || fn.name
           FROM tmp_pcg_formobj m
           JOIN files f ON f.path = m.form_path
           JOIN functions fn ON fn.file_id = f.id
           WHERE (' ' || IFNULL(fn.docstring, '') || ' ') LIKE '% &НаКлиенте %';",
    )?;

    // Квалификатор вызова не должен быть ни именем общего модуля (его резолвит
    // Tier C), ни коллекцией метаданных (её резолвит Tier D). Имена коллекций —
    // статические, инлайн безопасен (так же сделано для PLATFORM_BALAST).
    let colls = METADATA_COLLECTIONS
        .iter()
        .map(|c| format!("'{}'", c))
        .collect::<Vec<_>>()
        .join(",");
    let form_of_caller =
        format!("substr({edges}.caller_proc_key, 1, instr({edges}.caller_proc_key, '::') - 1)");
    let method =
        format!("substr({edges}.callee_proc_name, instr({edges}.callee_proc_name, '.') + 1)");
    let first =
        format!("substr({edges}.callee_proc_name, 1, instr({edges}.callee_proc_name, '.') - 1)");
    let single_dot = format!(
        "instr(substr({edges}.callee_proc_name, instr({edges}.callee_proc_name, '.') + 1), '.') = 0"
    );
    let exists = format!(
        "EXISTS ( \
           SELECT 1 FROM tmp_pcg_formobj m JOIN exported_procs e ON e.repo = ?1 AND e.path = m.obj_path \
           WHERE m.form_path = {form_of_caller} AND e.name = {method})"
    );
    let mut sql = format!(
        "UPDATE {edges} \
         SET callee_proc_key = ( \
             SELECT MIN(e.path || '::' || e.name) \
             FROM tmp_pcg_formobj m JOIN exported_procs e ON e.repo = ?1 AND e.path = m.obj_path \
             WHERE m.form_path = {form_of_caller} AND e.name = {method}) \
         WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
           AND instr({edges}.callee_proc_name, '.') > 0 \
           AND {single_dot} \
           AND {form_of_caller} IN (SELECT form_path FROM tmp_pcg_formobj) \
           AND {first} NOT IN (SELECT owner FROM exported_procs WHERE repo = ?1 AND kind = 'common' AND owner IS NOT NULL) \
           AND {first} NOT IN ({colls}) \
           AND {edges}.caller_proc_key NOT IN (SELECT k FROM tmp_pcg_formclient) \
           AND NOT ({first} LIKE 'Модуль%' AND substr({first}, 7) IN (SELECT owner FROM exported_procs WHERE repo = ?1 AND kind = 'common' AND owner IS NOT NULL)) \
           AND {exists}"
    );
    sql.push_str(scope.clause());
    conn.execute(&sql, params![REPO_DEFAULT])?;
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_formobj; DROP TABLE IF EXISTS tmp_pcg_formclient;",
    )?;
    Ok(())
}
