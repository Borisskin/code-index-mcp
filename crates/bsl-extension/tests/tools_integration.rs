// Интеграционные тесты для 1С MCP-tools (этап 6).
//
// Каждый тест:
//  1. Создаёт временную SQLite-БД с базовой схемой core +
//     bsl-extension schema_extensions.
//  2. Заполняет таблицы тестовыми данными (вручную или через
//     run_index_extras).
//  3. Вызывает соответствующий IndexTool::execute и проверяет
//     результат.
//
// Тесты в `tests/` потому что нужен полный API code-index-core
// (Storage, ToolContext) — внутри `bsl-extension` тяжело собрать
// все эти типы из-за приватных полей и Mutex<Storage>.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bsl_extension::{
    schema::SCHEMA_EXTENSIONS,
    tools::{
        FindDataPathTool, FindPathBslTool, FindUnreferencedObjectsTool, GetDataLinksTool,
        GetDcsSchemaTool, GetEventSubscriptionsTool, GetFormHandlersTool, GetObjectStructureTool,
        GetRegisterWritersTool, SearchTermsTool,
    },
};
use code_index_core::extension::{IndexTool, ToolContext};
use code_index_core::storage::{Storage, StoragePool};
use rusqlite::params;
use serde_json::Value;
use tempfile::TempDir;

const REPO: &str = "default";

fn fresh_storage() -> (TempDir, Arc<StoragePool>) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.db");
    let storage = Storage::open_file(&db_path).unwrap();
    storage.apply_schema_extensions(SCHEMA_EXTENSIONS).unwrap();
    (tmp, StoragePool::single(storage))
}

/// Запустить tool и вернуть **распакованное** значение `result` из ответа.
///
/// С 0.9.0 все extension-tools возвращают `{result, _meta: {dependent_files: [...]}}`
/// для event-based cache invalidation. Тесты проверяют поле `_meta` отдельно
/// (must exist и быть массивом), а основной result отдают наружу как раньше —
/// чтобы сохранить совместимость существующих assert'ов по `res["..."]`.
async fn run_tool(tool: &dyn IndexTool, storage: &Arc<StoragePool>, args: Value) -> Value {
    let ctx = ToolContext {
        repo: REPO,
        root_path: None,
        language: Some("bsl"),
        storage,
    };
    let raw = tool.execute(args, ctx).await;
    assert!(
        raw["_meta"]["dependent_files"].is_array(),
        "tool обязан возвращать _meta.dependent_files (массив, возможно пустой). Получили: {raw}"
    );
    raw["result"].clone()
}

// ── get_dcs_schema ────────────────────────────────────────────────────────

/// Схема компоновки: одна строка `dcs_schemas`, один набор и одно ребро
/// `dcs_query` на прочитанный объект.
async fn seed_dcs(storage: &Arc<StoragePool>) {
    let s = storage.get().await.unwrap();
    let conn = s.conn();
    conn.execute(
        "INSERT INTO dcs_schemas (repo, template_full_name, owner_full_name, title, content_file, \
         links_json, calculated_fields_json, totals_json, parameters_json, variants_json, \
         templates_count, data_sets_count, fields_count, links_count, calculated_fields_count, \
         totals_count, parameters_count, variants_count) \
         VALUES ('default', 'Report.Отчет.Template.Схема', 'Report.Отчет', 'Схема', \
         'Reports/Отчет/Templates/Схема/Ext/Template.xml', '[]', '[]', '[]', \
         '[{\"name\":\"Параметр1\"}]', '[]', 0, 1, 1, 0, 0, 0, 1, 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO dcs_datasets (repo, template_full_name, data_set_name, kind, data_source, \
         object_name, query_text, fields_json, parent_set) \
         VALUES ('default', 'Report.Отчет.Template.Схема', 'Набор1', 'query', 'ИсточникДанных1', \
         NULL, 'ВЫБРАТЬ Контрагент ИЗ Справочник.Контрагенты', '[{\"name\":\"Контрагент\"}]', '')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO data_links (repo, from_object, from_path, to_object, link_kind, is_composite, \
         is_universal, to_object_key) \
         VALUES ('default', 'Report.Отчет', 'Схема.Набор1', 'Catalog.Контрагенты', 'dcs_query', \
         0, 0, 'catalog.контрагенты')",
        [],
    )
    .unwrap();
}

#[tokio::test]
async fn get_dcs_schema_by_template_and_owner() {
    let (_tmp, storage) = fresh_storage();
    seed_dcs(&storage).await;

    // По имени макета.
    let res = run_tool(
        &GetDcsSchemaTool,
        &storage,
        serde_json::json!({"repo": REPO, "full_name": "Report.Отчет.Template.Схема"}),
    )
    .await;
    // Форма ответа одна и по макету, и по владельцу: список схем + их число.
    assert_eq!(res["schemas_count"].as_u64(), Some(1));
    let s = &res["schemas"][0];
    assert_eq!(s["owner"].as_str(), Some("Report.Отчет"));
    assert_eq!(
        s["template_full_name"].as_str(),
        Some("Report.Отчет.Template.Схема")
    );
    assert_eq!(s["data_sets"][0]["name"].as_str(), Some("Набор1"));
    assert!(s["data_sets"][0]["query"]
        .as_str()
        .unwrap_or_default()
        .contains("Справочник.Контрагенты"));
    assert!(s["data_sets"][0]["fields"].is_array());
    assert_eq!(s["reads"][0].as_str(), Some("Catalog.Контрагенты"));

    // По имени владельца (синоним ключа object).
    let res = run_tool(
        &GetDcsSchemaTool,
        &storage,
        serde_json::json!({"repo": REPO, "object": "Report.Отчет"}),
    )
    .await;
    assert_eq!(res["requested"].as_str(), Some("Report.Отчет"));
    assert_eq!(
        res["schemas"][0]["template_full_name"].as_str(),
        Some("Report.Отчет.Template.Схема")
    );

    // include_query=false → только длина запроса.
    let res = run_tool(
        &GetDcsSchemaTool,
        &storage,
        serde_json::json!({"repo": REPO, "template": "Report.Отчет.Template.Схема", "include_query": false}),
    )
    .await;
    let s = &res["schemas"][0];
    assert!(s["data_sets"][0]["query"].is_null());
    assert!(s["data_sets"][0]["query_length"].as_u64().unwrap_or(0) > 0);

    // sections=['parameters'] → наборов нет, параметры есть.
    let res = run_tool(
        &GetDcsSchemaTool,
        &storage,
        serde_json::json!({"repo": REPO, "name": "Report.Отчет.Template.Схема", "sections": ["parameters"]}),
    )
    .await;
    let s = &res["schemas"][0];
    assert!(s.get("data_sets").is_none());
    assert!(s["parameters"].is_array());
    // Опознание и счётчики остаются при любой выборке секций.
    assert!(s["data_sets_count"].is_number());
    assert_eq!(
        s["template_full_name"].as_str(),
        Some("Report.Отчет.Template.Схема")
    );

    // sections=['data_sets'] без 'fields' → наборы без полей, с их числом.
    let res = run_tool(
        &GetDcsSchemaTool,
        &storage,
        serde_json::json!({"repo": REPO, "full_name": "Report.Отчет.Template.Схема", "sections": ["data_sets"]}),
    )
    .await;
    let s = &res["schemas"][0];
    assert!(s["data_sets"][0].get("fields").is_none());
    assert!(s["data_sets"][0]["fields_count"].is_number());

    // «Не найдено» — ошибка и подсказка.
    let res = run_tool(
        &GetDcsSchemaTool,
        &storage,
        serde_json::json!({"repo": REPO, "full_name": "НичегоТакого"}),
    )
    .await;
    assert!(res["error"].is_string());
    assert!(res["hint"].is_string());
}

#[tokio::test]
async fn get_dcs_schema_truncates_under_budget() {
    let (_tmp, storage) = fresh_storage();
    seed_dcs(&storage).await;

    let res = run_tool(
        &GetDcsSchemaTool,
        &storage,
        serde_json::json!({
            "repo": REPO,
            "full_name": "Report.Отчет.Template.Схема",
            "max_response_bytes": 200
        }),
    )
    .await;
    // Текст запроса ушёл первым, но его длина и признак усечения на месте.
    let s = &res["schemas"][0];
    assert!(s["data_sets"][0]["query_length"].is_number());
    assert_eq!(s["data_sets"][0]["query_truncated"].as_bool(), Some(true));
    // Опознание и счётчики не урезаются никогда.
    assert_eq!(
        s["template_full_name"].as_str(),
        Some("Report.Отчет.Template.Схема")
    );
    assert!(s["content_file"].is_string());
    assert!(s["parameters_count"].is_number());
    // Подсказка называет ручку повтора.
    assert!(res["hint"]
        .as_str()
        .unwrap_or_default()
        .contains("max_response_bytes="));
}

// ── get_object_structure ──────────────────────────────────────────────────

#[tokio::test]
async fn get_object_structure_returns_existing() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.get().await.unwrap();
        s.conn()
            .execute(
                "INSERT INTO metadata_objects (repo, full_name, meta_type, name, synonym) \
                 VALUES (?, ?, ?, ?, ?)",
                params![
                    REPO,
                    "Catalog.Контрагенты",
                    "Catalog",
                    "Контрагенты",
                    "Контрагенты"
                ],
            )
            .unwrap();
    }
    let res = run_tool(
        &GetObjectStructureTool,
        &storage,
        serde_json::json!({"repo": REPO, "full_name": "Catalog.Контрагенты"}),
    )
    .await;
    assert_eq!(res["meta_type"].as_str(), Some("Catalog"));
    assert_eq!(res["name"].as_str(), Some("Контрагенты"));
    assert_eq!(res["synonym"].as_str(), Some("Контрагенты"));
}

// ── Регистр кириллицы в имени объекта (T-3) ───────────────────────────────
//
// Имена объектов 1С кириллические, и регистр в них легко перепутать. SQLite
// кириллицу не сворачивает, поэтому точное сравнение промахивалось МОЛЧА:
// связи отвечали «на объект никто не ссылается», регистраторы — «регистраторов
// нет». Ответ выглядел содержательным выводом об объекте — это опаснее ошибки.

/// Перечень объектов + связи для проверок регистра.
async fn seed_case_fixture(storage: &Arc<StoragePool>) {
    let s = storage.get().await.unwrap();
    let conn = s.conn();
    for (fqn, mt, nm) in [
        ("Catalog.Организации", "Catalog", "Организации"),
        (
            "Document.РеализацияТоваров",
            "Document",
            "РеализацияТоваров",
        ),
        (
            "AccumulationRegister.ВыручкаИСебестоимость",
            "AccumulationRegister",
            "ВыручкаИСебестоимость",
        ),
    ] {
        conn.execute(
            "INSERT INTO metadata_objects (repo, full_name, meta_type, name, synonym) \
             VALUES (?, ?, ?, ?, ?)",
            params![REPO, fqn, mt, nm, nm],
        )
        .unwrap();
    }
    bsl_extension::schema::backfill_metadata_object_keys(conn).unwrap();
    for (from, path, to, kind) in [
        (
            "Document.РеализацияТоваров",
            "Организация",
            "Catalog.Организации",
            "attr",
        ),
        (
            "Document.РеализацияТоваров",
            "Движения",
            "AccumulationRegister.ВыручкаИСебестоимость",
            "recorder",
        ),
    ] {
        conn.execute(
            "INSERT INTO data_links \
             (repo, from_object, from_path, to_object, link_kind, to_object_key) \
             VALUES (?, ?, ?, ?, ?, ?)",
            params![REPO, from, path, to, kind, to.to_lowercase()],
        )
        .unwrap();
    }
}

#[tokio::test]
async fn object_structure_found_despite_name_case() {
    let (_tmp, storage) = fresh_storage();
    seed_case_fixture(&storage).await;
    let res = run_tool(
        &GetObjectStructureTool,
        &storage,
        serde_json::json!({"repo": REPO, "full_name": "Catalog.организации"}),
    )
    .await;
    assert!(
        res["error"].is_null(),
        "имя с иным регистром должно резолвиться, а не давать ошибку: {res}"
    );
    assert_eq!(res["name"].as_str(), Some("Организации"));
}

#[tokio::test]
async fn object_structure_suggests_despite_name_case() {
    let (_tmp, storage) = fresh_storage();
    seed_case_fixture(&storage).await;
    // Объекта нет вовсе, но похожий отличается регистром: подбор идёт по ключу
    // в нижнем регистре, поэтому список подсказок не должен быть пустым.
    let res = run_tool(
        &GetObjectStructureTool,
        &storage,
        serde_json::json!({"repo": REPO, "full_name": "Catalog.организациии"}),
    )
    .await;
    assert!(res["error"].is_string());
    let dym = res["did_you_mean"]
        .as_array()
        .expect("did_you_mean должен быть массивом");
    assert!(
        dym.iter()
            .any(|v| v.as_str() == Some("Catalog.Организации")),
        "подсказка обязана предложить объект, отличающийся регистром: {res}"
    );
}

#[tokio::test]
async fn data_links_found_despite_name_case() {
    let (_tmp, storage) = fresh_storage();
    seed_case_fixture(&storage).await;
    let res = run_tool(
        &GetDataLinksTool,
        &storage,
        serde_json::json!({"repo": REPO, "object": "Catalog.организации", "direction": "in"}),
    )
    .await;
    assert_eq!(
        res["in_total"].as_u64(),
        Some(1),
        "иной регистр давал in_total: 0 — «на объект никто не ссылается»: {res}"
    );
    // В ответе имя показывается так, как записано в конфигурации.
    assert_eq!(res["object"].as_str(), Some("Catalog.Организации"));
}

#[tokio::test]
async fn data_links_out_found_despite_name_case() {
    let (_tmp, storage) = fresh_storage();
    seed_case_fixture(&storage).await;
    // Направление «на что ссылается» идёт по источнику ребра, у которого
    // колонки-ключа нет вовсе — резолв имени на входе закрывает и его.
    let res = run_tool(
        &GetDataLinksTool,
        &storage,
        serde_json::json!({"repo": REPO, "object": "document.реализациятоваров", "direction": "out"}),
    )
    .await;
    assert_eq!(res["out_total"].as_u64(), Some(2), "{res}");
}

#[tokio::test]
async fn register_writers_found_despite_name_case() {
    let (_tmp, storage) = fresh_storage();
    seed_case_fixture(&storage).await;
    let res = run_tool(
        &GetRegisterWritersTool,
        &storage,
        serde_json::json!({"repo": REPO, "object": "AccumulationRegister.выручкаисебестоимость"}),
    )
    .await;
    let writers = res["writers"].as_array().expect("writers — массив");
    assert_eq!(
        writers.len(),
        1,
        "иной регистр давал writers: [] — «у регистра нет регистраторов»: {res}"
    );
    assert_eq!(writers[0].as_str(), Some("Document.РеализацияТоваров"));
}

#[tokio::test]
async fn data_path_reports_depth_exhausted() {
    let (_tmp, storage) = fresh_storage();
    seed_case_fixture(&storage).await;
    {
        // Цепочка длиннее заданной глубины: путь есть, но за один шаг не виден.
        let s = storage.get().await.unwrap();
        s.conn()
            .execute(
                "INSERT INTO data_links \
                 (repo, from_object, from_path, to_object, link_kind, to_object_key) \
                 VALUES (?, 'Catalog.Организации', 'Владелец', 'Catalog.НетСвязи', 'attr', ?)",
                params![REPO, "catalog.нетсвязи"],
            )
            .unwrap();
    }
    let res = run_tool(
        &FindDataPathTool,
        &storage,
        serde_json::json!({
            "repo": REPO,
            "from": "Document.РеализацияТоваров",
            "to": "Catalog.НетСвязи",
            "max_depth": 1
        }),
    )
    .await;
    assert_eq!(res["found"].as_bool(), Some(false));
    assert_eq!(
        res["depth_exhausted"].as_bool(),
        Some(true),
        "«пути нет» обязано отличаться от «не хватило шагов»: {res}"
    );
    assert!(
        res["hint"].is_string(),
        "нужна подсказка со следующим вызовом: {res}"
    );

    // Той же глубины хватает — путь есть и признака обрыва нет.
    let ok = run_tool(
        &FindDataPathTool,
        &storage,
        serde_json::json!({
            "repo": REPO,
            "from": "Document.РеализацияТоваров",
            "to": "Catalog.НетСвязи",
            "max_depth": 2
        }),
    )
    .await;
    assert_eq!(ok["found"].as_bool(), Some(true), "{ok}");
}

#[tokio::test]
async fn get_object_structure_reports_missing() {
    let (_tmp, storage) = fresh_storage();
    let res = run_tool(
        &GetObjectStructureTool,
        &storage,
        serde_json::json!({"repo": REPO, "full_name": "Catalog.НетТакого"}),
    )
    .await;
    assert!(res["error"].is_string());
}

#[tokio::test]
async fn get_object_structure_validates_full_name() {
    let (_tmp, storage) = fresh_storage();
    let res = run_tool(
        &GetObjectStructureTool,
        &storage,
        serde_json::json!({"repo": REPO}), // нет full_name
    )
    .await;
    assert!(
        res["error"].as_str().unwrap_or("").contains("full_name"),
        "ошибка должна упоминать missing full_name: {:?}",
        res
    );
}

#[tokio::test]
async fn get_object_structure_batch_full_names() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.get().await.unwrap();
        for (fqn, mt, nm) in [
            ("Catalog.Контрагенты", "Catalog", "Контрагенты"),
            (
                "Document.РеализацияТоваровУслуг",
                "Document",
                "РеализацияТоваровУслуг",
            ),
        ] {
            s.conn()
                .execute(
                    "INSERT INTO metadata_objects (repo, full_name, meta_type, name, synonym) \
                     VALUES (?, ?, ?, ?, ?)",
                    params![REPO, fqn, mt, nm, nm],
                )
                .unwrap();
        }
    }
    let res = run_tool(
        &GetObjectStructureTool,
        &storage,
        serde_json::json!({
            "repo": REPO,
            "full_names": [
                "Catalog.Контрагенты",
                "Document.РеализацияТоваровУслуг",
                "Catalog.НетТакого"
            ]
        }),
    )
    .await;
    let results = res["results"]
        .as_array()
        .expect("массовый режим должен вернуть массив results");
    assert_eq!(
        results.len(),
        3,
        "три запрошенных объекта — три результата по порядку"
    );
    assert_eq!(results[0]["meta_type"].as_str(), Some("Catalog"));
    assert_eq!(results[0]["name"].as_str(), Some("Контрагенты"));
    assert_eq!(results[1]["meta_type"].as_str(), Some("Document"));
    // несуществующий объект → свой слот с error + did_you_mean, не валит весь батч
    assert!(
        results[2]["error"].is_string(),
        "несуществующий объект должен дать error в своём слоте: {:?}",
        results[2]
    );
    assert!(results[2]["did_you_mean"].is_array());
}

#[tokio::test]
async fn get_object_structure_batch_non_string_element() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.get().await.unwrap();
        s.conn()
            .execute(
                "INSERT INTO metadata_objects (repo, full_name, meta_type, name, synonym) \
                 VALUES (?, ?, ?, ?, ?)",
                params![
                    REPO,
                    "Catalog.Контрагенты",
                    "Catalog",
                    "Контрагенты",
                    "Контрагенты"
                ],
            )
            .unwrap();
    }
    let res = run_tool(
        &GetObjectStructureTool,
        &storage,
        serde_json::json!({
            "repo": REPO,
            "full_names": ["Catalog.Контрагенты", 42, "Catalog.Контрагенты"]
        }),
    )
    .await;
    let results = res["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["meta_type"].as_str(), Some("Catalog"));
    // нестроковый элемент → {error} строго на своей позиции, соседи не страдают
    assert!(
        results[1]["error"]
            .as_str()
            .unwrap_or("")
            .contains("строкой"),
        "нестроковый элемент должен дать error на своей позиции: {:?}",
        results[1]
    );
    assert_eq!(results[2]["meta_type"].as_str(), Some("Catalog"));
}

#[tokio::test]
async fn get_object_structure_batch_empty_list() {
    let (_tmp, storage) = fresh_storage();
    let res = run_tool(
        &GetObjectStructureTool,
        &storage,
        serde_json::json!({"repo": REPO, "full_names": []}),
    )
    .await;
    let results = res["results"]
        .as_array()
        .expect("пустой батч → пустой results");
    assert!(results.is_empty());
}

// ── get_form_handlers ─────────────────────────────────────────────────────

#[tokio::test]
async fn get_form_handlers_returns_array() {
    let (_tmp, storage) = fresh_storage();
    let handlers_json = serde_json::json!([
        {"event": "ПриОткрытии", "handler": "ПриОткрытии"},
        {"event": "ПередЗакрытием", "handler": "ОбработатьЗакрытие"}
    ])
    .to_string();
    {
        let s = storage.get().await.unwrap();
        // Боевой формат хранения — папка выгрузки (plural): 'Documents.X'.
        s.conn()
            .execute(
                "INSERT INTO metadata_forms (repo, owner_full_name, form_name, handlers_json) \
                 VALUES (?, ?, ?, ?)",
                params![
                    REPO,
                    "Documents.Реализация",
                    "ФормаДокумента",
                    handlers_json
                ],
            )
            .unwrap();
    }
    // Запрос в singular-формате ('Document.X') должен найти строку через
    // нормализацию meta_type_to_folder.
    let res = run_tool(
        &GetFormHandlersTool,
        &storage,
        serde_json::json!({
            "repo": REPO,
            "owner_full_name": "Document.Реализация",
            "form_name": "ФормаДокумента"
        }),
    )
    .await;
    let arr = res["handlers"].as_array().expect("handlers — массив");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["event"].as_str(), Some("ПриОткрытии"));
    assert_eq!(arr[1]["handler"].as_str(), Some("ОбработатьЗакрытие"));
    assert_eq!(
        res["owner_full_name"].as_str(),
        Some("Documents.Реализация"),
        "в ответе — реально сматченный ключ БД"
    );

    // Plural-формат принимается как есть.
    let res2 = run_tool(
        &GetFormHandlersTool,
        &storage,
        serde_json::json!({
            "repo": REPO,
            "owner_full_name": "Documents.Реализация",
            "form_name": "ФормаДокумента"
        }),
    )
    .await;
    assert_eq!(
        res2["handlers"].as_array().map(|a| a.len()),
        Some(2),
        "plural-формат тоже находит форму"
    );
}

#[tokio::test]
async fn get_form_handlers_unknown_form_lists_available() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.get().await.unwrap();
        for form in &["ФормаДокумента", "ФормаСписка"] {
            s.conn()
                .execute(
                    "INSERT INTO metadata_forms (repo, owner_full_name, form_name, handlers_json) \
                     VALUES (?, ?, ?, '[]')",
                    params![REPO, "Documents.Реализация", form],
                )
                .unwrap();
        }
    }
    // Форма не существует → error + available_forms с реальными формами владельца.
    let res = run_tool(
        &GetFormHandlersTool,
        &storage,
        serde_json::json!({
            "repo": REPO,
            "owner_full_name": "Document.Реализация",
            "form_name": "НетТакойФормы"
        }),
    )
    .await;
    assert!(res["error"]
        .as_str()
        .unwrap_or("")
        .contains("form not found"));
    let available: Vec<&str> = res["available_forms"]
        .as_array()
        .expect("available_forms — массив")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(available, vec!["ФормаДокумента", "ФормаСписка"]);

    // Владелец не существует → error + hint про формат, без available_forms.
    let res2 = run_tool(
        &GetFormHandlersTool,
        &storage,
        serde_json::json!({
            "repo": REPO,
            "owner_full_name": "Document.НетТакого",
            "form_name": "ФормаДокумента"
        }),
    )
    .await;
    assert!(res2["error"]
        .as_str()
        .unwrap_or("")
        .contains("form not found"));
    assert!(res2["hint"].as_str().unwrap_or("").contains("Document.X"));
    assert!(res2["available_forms"].is_null());
}

// ── get_event_subscriptions ───────────────────────────────────────────────

#[tokio::test]
async fn get_event_subscriptions_lists_all_when_no_filters() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.get().await.unwrap();
        for (name, ev, m, p) in &[
            ("Sub1", "ПриЗаписи", "Логирование", "Записать"),
            ("Sub2", "ПередЗаписью", "Аудит", "Проверить"),
        ] {
            s.conn()
                .execute(
                    "INSERT INTO event_subscriptions (repo, name, event, handler_module, handler_proc, sources_json) \
                     VALUES (?, ?, ?, ?, ?, '[]')",
                    params![REPO, name, ev, m, p],
                )
                .unwrap();
        }
    }
    let res = run_tool(
        &GetEventSubscriptionsTool,
        &storage,
        serde_json::json!({"repo": REPO}),
    )
    .await;
    assert_eq!(res["count"].as_u64(), Some(2));
    assert_eq!(res["subscriptions"].as_array().map(|a| a.len()), Some(2));
}

#[tokio::test]
async fn get_event_subscriptions_filters_by_handler_module() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.get().await.unwrap();
        for (name, m) in &[("A", "ModX"), ("B", "ModY"), ("C", "ModX")] {
            s.conn()
                .execute(
                    "INSERT INTO event_subscriptions (repo, name, event, handler_module, handler_proc, sources_json) \
                     VALUES (?, ?, 'E', ?, 'P', '[]')",
                    params![REPO, name, m],
                )
                .unwrap();
        }
    }
    let res = run_tool(
        &GetEventSubscriptionsTool,
        &storage,
        serde_json::json!({"repo": REPO, "handler_module": "ModX"}),
    )
    .await;
    assert_eq!(res["count"].as_u64(), Some(2), "filter ModX → 2 совпадения");
}

#[tokio::test]
async fn get_event_subscriptions_filters_by_source() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.get().await.unwrap();
        for (name, sources) in &[
            ("SubZakaz", r#"["cfg:DocumentObject.ЗаказКлиента"]"#),
            ("SubVariant", r#"["cfg:CatalogObject.ВариантыОтчетов"]"#),
            (
                "SubMulti",
                r#"["cfg:CatalogObject.Контрагенты","cfg:DocumentObject.ЗаказКлиента"]"#,
            ),
        ] {
            s.conn()
                .execute(
                    "INSERT INTO event_subscriptions (repo, name, event, handler_module, handler_proc, sources_json) \
                     VALUES (?, ?, 'ПередЗаписью', 'M', 'P', ?)",
                    params![REPO, name, sources],
                )
                .unwrap();
        }
    }
    // Полное имя в singular-формате → матчит DocumentObject-источник.
    let res = run_tool(
        &GetEventSubscriptionsTool,
        &storage,
        serde_json::json!({"repo": REPO, "source": "Document.ЗаказКлиента"}),
    )
    .await;
    assert_eq!(
        res["count"].as_u64(),
        Some(2),
        "ЗаказКлиента в двух подписках"
    );
    assert_eq!(res["total"].as_u64(), Some(2));

    // Короткое имя (без типа) — тот же результат, регистр игнорируется.
    let res2 = run_tool(
        &GetEventSubscriptionsTool,
        &storage,
        serde_json::json!({"repo": REPO, "source": "заказклиента"}),
    )
    .await;
    assert_eq!(res2["count"].as_u64(), Some(2));

    // Несуществующий источник → пусто.
    let res3 = run_tool(
        &GetEventSubscriptionsTool,
        &storage,
        serde_json::json!({"repo": REPO, "source": "Document.НетТакого"}),
    )
    .await;
    assert_eq!(res3["count"].as_u64(), Some(0));
}

#[tokio::test]
async fn get_event_subscriptions_rejects_unknown_param() {
    let (_tmp, storage) = fresh_storage();
    let res = run_tool(
        &GetEventSubscriptionsTool,
        &storage,
        serde_json::json!({"repo": REPO, "object": "ЗаказКлиента"}),
    )
    .await;
    let err = res["error"].as_str().unwrap_or("");
    assert!(
        err.contains("object"),
        "ошибка называет неизвестный параметр: {}",
        err
    );
    assert!(
        res["hint"].as_str().unwrap_or("").contains("source"),
        "hint перечисляет допустимые фильтры"
    );
}

// ── find_path_bsl ───────────────────────────────────────────────────────────

#[tokio::test]
async fn find_path_returns_direct_edge_when_exists() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.get().await.unwrap();
        s.conn()
            .execute(
                "INSERT INTO proc_call_graph (repo, caller_proc_key, callee_proc_name, call_type) \
                 VALUES (?, 'A', 'B', 'direct')",
                params![REPO],
            )
            .unwrap();
    }
    let res = run_tool(
        &FindPathBslTool,
        &storage,
        serde_json::json!({"repo": REPO, "from": "A", "to": "B", "max_depth": 3}),
    )
    .await;
    assert_eq!(res["found"].as_bool(), Some(true));
    let path = res["path"].as_array().expect("path — массив");
    assert_eq!(path.len(), 1);
    assert_eq!(path[0]["caller"].as_str(), Some("A"));
    assert_eq!(path[0]["callee"].as_str(), Some("B"));
}

#[tokio::test]
async fn find_path_walks_two_hops() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.get().await.unwrap();
        s.conn()
            .execute(
                "INSERT INTO proc_call_graph (repo, caller_proc_key, callee_proc_name, call_type) \
                 VALUES (?, 'A', 'B', 'direct'), (?, 'B', 'C', 'direct')",
                params![REPO, REPO],
            )
            .unwrap();
    }
    let res = run_tool(
        &FindPathBslTool,
        &storage,
        serde_json::json!({"repo": REPO, "from": "A", "to": "C", "max_depth": 3}),
    )
    .await;
    assert_eq!(
        res["found"].as_bool(),
        Some(true),
        "путь A→B→C должен находиться"
    );
    let path = res["path"].as_array().unwrap();
    assert_eq!(path.len(), 2);
    assert_eq!(
        res["from_key"].as_str(),
        Some("A"),
        "from_key — начало пути, а не промежуточный вызывающий"
    );
}

/// Голое имя разворачивается в несколько стартовых ключей: `from_key` должен
/// назвать тот, с которого реально начался найденный путь (регресс: отдавался
/// вызывающий последнего ребра).
#[tokio::test]
async fn find_path_from_key_is_start_of_found_path() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.get().await.unwrap();
        s.conn()
            .execute(
                "INSERT INTO proc_call_graph (repo, caller_proc_key, callee_proc_name, callee_proc_key, call_type) VALUES \
                 (?1, 'm1.bsl::Старт', 'Икс', 'm1.bsl::Икс', 'direct'), \
                 (?1, 'm2.bsl::Старт', 'Промежуточная', 'm2.bsl::Промежуточная', 'direct'), \
                 (?1, 'm2.bsl::Промежуточная', 'Цель', 'm3.bsl::Цель', 'direct')",
                params![REPO],
            )
            .unwrap();
    }
    let res = run_tool(
        &FindPathBslTool,
        &storage,
        serde_json::json!({"repo": REPO, "from": "Старт", "to": "Цель", "max_depth": 3}),
    )
    .await;
    assert_eq!(res["found"].as_bool(), Some(true));
    assert_eq!(res["from_key"].as_str(), Some("m2.bsl::Старт"));
    let path = res["path"].as_array().unwrap();
    assert_eq!(path.len(), 2);
    assert_eq!(path[0]["caller"].as_str(), Some("m2.bsl::Старт"));
}

#[tokio::test]
async fn find_path_returns_not_found_when_no_path() {
    let (_tmp, storage) = fresh_storage();
    let res = run_tool(
        &FindPathBslTool,
        &storage,
        serde_json::json!({"repo": REPO, "from": "A", "to": "B"}),
    )
    .await;
    assert_eq!(res["found"].as_bool(), Some(false));
    assert!(res["path"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn find_path_respects_max_depth() {
    let (_tmp, storage) = fresh_storage();
    {
        // Цепочка A → B → C → D, max_depth=2 → не должен найти D (длина 3).
        let s = storage.get().await.unwrap();
        s.conn()
            .execute(
                "INSERT INTO proc_call_graph (repo, caller_proc_key, callee_proc_name, call_type) VALUES \
                 (?, 'A', 'B', 'direct'), (?, 'B', 'C', 'direct'), (?, 'C', 'D', 'direct')",
                params![REPO, REPO, REPO],
            )
            .unwrap();
    }
    let res_short = run_tool(
        &FindPathBslTool,
        &storage,
        serde_json::json!({"repo": REPO, "from": "A", "to": "D", "max_depth": 2}),
    )
    .await;
    assert_eq!(res_short["found"].as_bool(), Some(false));

    let res_full = run_tool(
        &FindPathBslTool,
        &storage,
        serde_json::json!({"repo": REPO, "from": "A", "to": "D", "max_depth": 3}),
    )
    .await;
    assert_eq!(res_full["found"].as_bool(), Some(true));
}

// ── search_terms (этап 5a) ────────────────────────────────────────────────

/// Хелпер: засеять `procedure_enrichment` тремя записями для тестов.
async fn seed_enrichment(storage: &Arc<StoragePool>) {
    let s = storage.get().await.unwrap();
    let conn = s.conn();
    for (proc_key, terms) in &[
        ("Расчёт.Старт", "запуск, инициализация, проведение"),
        (
            "Продажи.СоздатьЗаказ",
            "товары, склад, заказ клиента, скидки",
        ),
        ("Логирование.Записать", "журнал, аудит, ошибка, отладка"),
    ] {
        conn.execute(
            "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
             VALUES (?, ?, ?, ?, ?)",
            params![
                REPO,
                proc_key,
                terms,
                "openai_compatible:claude-haiku-4.5",
                0i64
            ],
        )
        .unwrap();
    }
}

#[tokio::test]
async fn search_terms_multiword_rewritten_to_or() {
    // Многословный запрос без операторов — серверный OR-rewrite (фикс по
    // бенчу 2026-06-10: неявный AND давал 0 на коротких термах). Слова
    // «склад» и «журнал» лежат в РАЗНЫХ записях — AND дал бы 0, OR находит обе.
    let (_tmp, storage) = fresh_storage();
    seed_enrichment(&storage).await;

    let res = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "склад журнал недостижимое"}),
    )
    .await;
    assert_eq!(
        res["fts_query"].as_str(),
        Some("\"склад\" OR \"журнал\" OR \"недостижимое\"")
    );
    let keys: Vec<&str> = res["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["proc_key"].as_str())
        .collect();
    assert_eq!(keys.len(), 2, "OR должен найти обе записи: {keys:?}");
    assert!(keys.contains(&"Продажи.СоздатьЗаказ"));
    assert!(keys.contains(&"Логирование.Записать"));

    // Ё в запросе сворачивается: «учёт» ищется как «учет».
    let res_yo = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "учёт"}),
    )
    .await;
    assert_eq!(res_yo["fts_query"].as_str(), Some("\"учет\""));
}

#[tokio::test]
async fn search_terms_finds_by_simple_word() {
    let (_tmp, storage) = fresh_storage();
    seed_enrichment(&storage).await;

    let res = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "склад"}),
    )
    .await;
    let results = res["results"].as_array().expect("results — массив");
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0]["proc_key"].as_str(),
        Some("Продажи.СоздатьЗаказ")
    );
    assert!(results[0]["terms"].as_str().unwrap().contains("склад"));
    assert!(results[0]["signature"].as_str().is_some());
    // BM25 ранжирование возвращает отрицательные числа (меньше = лучше).
    assert!(results[0]["score"].as_f64().is_some());
}

#[tokio::test]
async fn search_terms_supports_and_or() {
    let (_tmp, storage) = fresh_storage();
    seed_enrichment(&storage).await;

    // OR: «склад OR журнал» должно найти и Продажи, и Логирование.
    let res_or = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "склад OR журнал"}),
    )
    .await;
    let or_keys: Vec<String> = res_or["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["proc_key"].as_str().map(String::from))
        .collect();
    assert_eq!(or_keys.len(), 2);
    assert!(or_keys.contains(&"Продажи.СоздатьЗаказ".to_string()));
    assert!(or_keys.contains(&"Логирование.Записать".to_string()));

    // AND: «товары AND склад» — только Продажи.
    let res_and = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "товары AND склад"}),
    )
    .await;
    let and_results = res_and["results"].as_array().unwrap();
    assert_eq!(and_results.len(), 1);
    assert_eq!(
        and_results[0]["proc_key"].as_str(),
        Some("Продажи.СоздатьЗаказ")
    );
}

#[tokio::test]
async fn search_terms_returns_empty_for_unknown_word() {
    let (_tmp, storage) = fresh_storage();
    seed_enrichment(&storage).await;

    let res = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "квантовая_телепортация"}),
    )
    .await;
    let results = res["results"].as_array().unwrap();
    assert!(
        results.is_empty(),
        "слово, которого нет в termах, не должно совпадать"
    );
}

#[tokio::test]
async fn search_terms_filters_by_repo() {
    // Запись в другой repo не должна находиться по нашему alias.
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.get().await.unwrap();
        s.conn()
            .execute(
                "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
                 VALUES (?, ?, ?, ?, ?)",
                params!["other-repo", "X.Y", "складские, операции", "sig", 0i64],
            )
            .unwrap();
    }
    let res = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "складские"}),
    )
    .await;
    let results = res["results"].as_array().unwrap();
    assert!(
        results.is_empty(),
        "запись из другого repo не должна находиться"
    );
}

#[tokio::test]
async fn search_terms_empty_query_returns_error() {
    let (_tmp, storage) = fresh_storage();
    let res = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "   "}),
    )
    .await;
    assert!(
        res["error"].as_str().is_some(),
        "пустой query должен возвращать error"
    );
}

#[tokio::test]
async fn search_terms_respects_limit() {
    let (_tmp, storage) = fresh_storage();
    seed_enrichment(&storage).await;
    // Все три записи содержат «и» в окончаниях, но точнее найдём по «проведение/заказ/журнал»:
    // «OR» через всё → 3 записи; ставим limit=2.
    let res = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "проведение OR заказ OR журнал", "limit": 2}),
    )
    .await;
    let results = res["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
}

// ── Привязки обработчиков форм (declarative_callers) ──────────────────────
//
// Обработчик формы объявлен в модуле, но в коде его никто не вызывает —
// выполняет его платформа по записи в описании формы. `get_callers` без этого
// отдавал ноль рёбер, и ответ читался как «процедура нигде не используется».

/// Форма с одним обработчиком: модуль, описание и запись в `metadata_forms`.
async fn seed_form_with_handler(
    storage: &Arc<StoragePool>,
    owner_folder: &str,
    object: &str,
    form: &str,
    handler: &str,
) {
    let s = storage.get().await.unwrap();
    let conn = s.conn();
    let form_root = format!("base/{}/{}/Forms/{}", owner_folder, object, form);
    let module_path = format!("{}/Ext/Form/Module.bsl", form_root);
    let descriptor_path = format!("{}/Ext/Form.xml", form_root);
    for (path, lang) in [(&module_path, "bsl"), (&descriptor_path, "text")] {
        conn.execute(
            "INSERT INTO files (path, content_hash, language) VALUES (?, ?, ?)",
            params![path, "hash", lang],
        )
        .unwrap();
    }
    let file_id: i64 = conn
        .query_row(
            "SELECT id FROM files WHERE path = ?",
            params![&module_path],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO functions (file_id, name, line_start, line_end) VALUES (?, ?, ?, ?)",
        params![file_id, handler, 10, 20],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO metadata_forms (repo, owner_full_name, form_name, handlers_json) \
         VALUES (?, ?, ?, ?)",
        params![
            REPO,
            format!("{}.{}", owner_folder, object),
            form,
            serde_json::json!([{"event": "ПередЗаписьюНаСервере", "handler": handler}]).to_string()
        ],
    )
    .unwrap();
}

#[tokio::test]
async fn form_handler_binding_is_reported_as_caller() {
    let (_tmp, storage) = fresh_storage();
    seed_form_with_handler(
        &storage,
        "Catalogs",
        "Контрагенты",
        "ФормаЭлемента",
        "ПередЗаписью1",
    )
    .await;

    let s = storage.get().await.unwrap();
    let found = bsl_extension::form_bindings::form_bindings(&s, "ПередЗаписью1");
    assert_eq!(
        found.len(),
        1,
        "привязка обработчика должна находиться: {found:?}"
    );
    assert_eq!(found[0]["kind"].as_str(), Some("form_binding"));
    assert_eq!(found[0]["event"].as_str(), Some("ПередЗаписьюНаСервере"));
    assert_eq!(
        found[0]["path"].as_str(),
        Some("base/Catalogs/Контрагенты/Forms/ФормаЭлемента/Ext/Form.xml"),
        "путь описания нужен для сброса кэша по файлу"
    );
    assert!(
        found[0]["caller"]
            .as_str()
            .unwrap()
            .contains("ФормаЭлемента"),
        "в caller должна быть видна форма"
    );
}

#[tokio::test]
async fn same_handler_name_in_other_form_is_not_mixed_in() {
    // Имена вроде «ПриОткрытии» встречаются в сотнях форм. Связь идёт по файлу
    // модуля, поэтому чужая форма с тем же именем обработчика подмешаться
    // не должна — иначе выдача полна ложных привязок.
    let (_tmp, storage) = fresh_storage();
    seed_form_with_handler(
        &storage,
        "Catalogs",
        "Контрагенты",
        "ФормаЭлемента",
        "ПриОткрытии",
    )
    .await;
    {
        // Вторая форма: запись в metadata_forms с тем же обработчиком есть,
        // а объявления процедуры в её модуле нет.
        let s = storage.get().await.unwrap();
        s.conn()
            .execute(
                "INSERT INTO metadata_forms (repo, owner_full_name, form_name, handlers_json) \
                 VALUES (?, ?, ?, ?)",
                params![
                    REPO,
                    "Documents.Заказ",
                    "ФормаДокумента",
                    serde_json::json!([{"event": "ПриОткрытии", "handler": "ПриОткрытии"}])
                        .to_string()
                ],
            )
            .unwrap();
    }

    let s = storage.get().await.unwrap();
    let found = bsl_extension::form_bindings::form_bindings(&s, "ПриОткрытии");
    assert_eq!(found.len(), 1, "должна найтись ровно одна форма: {found:?}");
    assert!(
        found[0]["caller"].as_str().unwrap().contains("Контрагенты"),
        "найдена не та форма: {found:?}"
    );
}

#[tokio::test]
async fn shared_event_name_yields_one_record_pointing_at_get_form_handlers() {
    // Имя `ПриОткрытии` носят разные процедуры в разных формах, у каждой своя
    // привязка. Список из тысяч форм ничего не говорит о нужной процедуре,
    // поэтому вместо него — одна запись со счётом и готовым следующим вызовом.
    let (_tmp, storage) = fresh_storage();
    for n in 0..12 {
        seed_form_with_handler(
            &storage,
            "Catalogs",
            &format!("Объект{n}"),
            "ФормаЭлемента",
            "ПриОткрытии",
        )
        .await;
    }
    let s = storage.get().await.unwrap();
    let found = bsl_extension::form_bindings::form_bindings(&s, "ПриОткрытии");
    assert_eq!(found.len(), 1, "ожидаем одну сводную запись: {found:?}");
    assert_eq!(found[0]["kind"].as_str(), Some("form_binding_ambiguous"));
    let hint = found[0]["hint"].as_str().unwrap();
    assert!(
        hint.contains(
            "get_form_handlers(owner_full_name='Catalogs.Объект0', form_name='ФормаЭлемента')"
        ),
        "подсказка должна нести образец вызова: {hint}"
    );
}

#[tokio::test]
async fn same_name_in_form_modules_without_binding_is_not_reported() {
    // Одноимённые процедуры в модулях форм — ещё НЕ привязка: имя может просто
    // повторяться (в формах объявлена своя функция с именем функции общего
    // модуля). Утверждать «у каждой своя привязка», не сверившись с описанием
    // формы, нельзя — это выдача догадки за факт.
    let (_tmp, storage) = fresh_storage();
    let s = storage.get().await.unwrap();
    let conn = s.conn();
    for n in 0..4 {
        let module_path =
            format!("base/Catalogs/Объект{n}/Forms/ФормаЭлемента/Ext/Form/Module.bsl");
        conn.execute(
            "INSERT INTO files (path, content_hash, language) VALUES (?, ?, ?)",
            params![&module_path, "hash", "bsl"],
        )
        .unwrap();
        let file_id: i64 = conn
            .query_row(
                "SELECT id FROM files WHERE path = ?",
                params![&module_path],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO functions (file_id, name, line_start, line_end) VALUES (?, ?, ?, ?)",
            params![file_id, "ЗначениеРеквизитаОбъекта", 1, 5],
        )
        .unwrap();
        // Форма в индексе есть, но этой процедуры среди её обработчиков нет.
        conn.execute(
            "INSERT INTO metadata_forms (repo, owner_full_name, form_name, handlers_json) \
             VALUES (?, ?, ?, ?)",
            params![
                REPO,
                format!("Catalogs.Объект{n}"),
                "ФормаЭлемента",
                serde_json::json!([{"event": "ПриОткрытии", "handler": "ПриОткрытии"}]).to_string()
            ],
        )
        .unwrap();
    }
    let found = bsl_extension::form_bindings::form_bindings(&s, "ЗначениеРеквизитаОбъекта");
    assert!(
        found.is_empty(),
        "привязок нет — и говорить о них нельзя: {found:?}"
    );
}

#[tokio::test]
async fn ordinary_procedure_has_no_form_bindings() {
    // Процедура общего модуля к формам отношения не имеет — привязок нет,
    // и лишних запросов быть не должно.
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.get().await.unwrap();
        let conn = s.conn();
        conn.execute(
            "INSERT INTO files (path, content_hash, language) VALUES (?, ?, ?)",
            params![
                "base/CommonModules/ОбщегоНазначения/Ext/Module.bsl",
                "hash",
                "bsl"
            ],
        )
        .unwrap();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files LIMIT 1", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO functions (file_id, name, line_start, line_end) VALUES (?, ?, ?, ?)",
            params![file_id, "ЗначениеРеквизита", 1, 5],
        )
        .unwrap();
    }
    let s = storage.get().await.unwrap();
    assert!(bsl_extension::form_bindings::form_bindings(&s, "ЗначениеРеквизита").is_empty());
}

// ── find_unreferenced_objects (Требование 2 ТЗ, C2) ────────────────────────

/// Пометить индекс собранным текущим бинарником: свежая тестовая БД номера не
/// содержит и штатно считается устаревшей (`stale`) — инструмент тогда чисел
/// не отдаёт (Требование 0).
async fn mark_index_current(storage: &Arc<StoragePool>) {
    let s = storage.get().await.unwrap();
    s.set_data_version(Storage::INDEX_DATA_VERSION).unwrap();
}

#[tokio::test]
async fn find_unreferenced_objects_stale_index_returns_no_numbers() {
    // Номер версии данных не проставлен → база устаревшая (Требование 0).
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.get().await.unwrap();
        let conn = s.conn();
        conn.execute(
            "INSERT INTO metadata_objects (repo, full_name, meta_type, name) \
             VALUES ('default','Catalog.Нигде','Catalog','Нигде')",
            [],
        )
        .unwrap();
        bsl_extension::schema::backfill_metadata_object_keys(conn).unwrap();
    }

    let res = run_tool(
        &FindUnreferencedObjectsTool,
        &storage,
        serde_json::json!({"repo": REPO}),
    )
    .await;

    assert_eq!(res["stale"].as_bool(), Some(true), "{res}");
    assert!(
        res["results"].is_null(),
        "при stale числа не отдаются: {res}"
    );
    assert!(res["results_total"].is_null(), "{res}");
    assert!(res["hint"]
        .as_str()
        .unwrap_or("")
        .contains("прежней версией данных"));
}

#[tokio::test]
async fn find_unreferenced_objects_reports_channels_and_paginates() {
    let (_tmp, storage) = fresh_storage();
    mark_index_current(&storage).await;
    {
        let s = storage.get().await.unwrap();
        let conn = s.conn();
        for (fqn, mt, nm) in [
            ("Catalog.Вакансии", "Catalog", "Вакансии"),
            (
                "Catalog.Вакансии.Template.ПФ_MXL_Вакансия",
                "Template",
                "ПФ_MXL_Вакансия",
            ),
            (
                "Catalog.Новости.Template._СтандартныеЗначения_ЗдесьУказатьКодКанала",
                "Template",
                "_СтандартныеЗначения_ЗдесьУказатьКодКанала",
            ),
            ("Subsystem.Продажи", "Subsystem", "Продажи"),
        ] {
            conn.execute(
                "INSERT INTO metadata_objects (repo, full_name, meta_type, name) \
                 VALUES (?, ?, ?, ?)",
                params![REPO, fqn, mt, nm],
            )
            .unwrap();
        }
        bsl_extension::schema::backfill_metadata_object_keys(conn).unwrap();
        // Строковый путь метаданных в коде — как его кладёт extract_code_usages.
        conn.execute(
            "INSERT INTO metadata_code_usages \
             (repo, object_ref, object_ref_key, member_path, usage_kind, file_path, line) \
             VALUES ('default','Catalog.Вакансии','catalog.вакансии','ПФ_MXL_Вакансия', \
                     'query','Catalogs/Вакансии/Ext/ManagerModule.bsl',58)",
            [],
        )
        .unwrap();
    }

    let res = run_tool(
        &FindUnreferencedObjectsTool,
        &storage,
        serde_json::json!({"repo": REPO, "max_response_bytes": 400}),
    )
    .await;

    assert_eq!(res["formula_version"].as_str(), Some("c2-v3"));
    assert!(
        res["channels_applied"]["template_paths"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    );
    assert_eq!(res["template_call_sites_unresolved"].as_u64(), Some(0));
    assert!(res["not_addressable"]["Subsystem"].as_u64().unwrap_or(0) >= 1);
    // Кандидат — только неиспользуемый макет: сам справочник снят каналом
    // code_usages (строка пути — обращение к нему), используемый — template_paths.
    let total = res["results_total"].as_u64().unwrap_or(0);
    assert!(total >= 1, "{res}");
    let mut seen: Vec<String> = res["results"]
        .as_array()
        .expect("results — массив")
        .iter()
        .filter_map(|r| r["full_name"].as_str().map(String::from))
        .collect();
    assert!(
        !seen.is_empty(),
        "массив не обнуляется даже при малом бюджете"
    );
    assert!(
        !seen.contains(&"Catalog.Вакансии.Template.ПФ_MXL_Вакансия".to_string()),
        "используемый макет не должен попасть в список: {seen:?}"
    );
    assert!(res["hint"]
        .as_str()
        .unwrap_or("")
        .contains("НЕ доказательство"));

    // Догрузка страниц по маркерам: набор собирается целиком.
    let mut offset =
        res["results_offset"].as_u64().unwrap_or(0) + res["results_shown"].as_u64().unwrap_or(0);
    let mut guard = 0;
    while seen.len() < total as usize && guard < 10 {
        guard += 1;
        let page = run_tool(
            &FindUnreferencedObjectsTool,
            &storage,
            serde_json::json!({"repo": REPO, "max_response_bytes": 400, "offset": offset}),
        )
        .await;
        assert_eq!(page["results_total"].as_u64(), Some(total));
        let items = page["results"].as_array().expect("results — массив");
        assert!(!items.is_empty(), "страница не пуста, пока есть остаток");
        seen.extend(
            items
                .iter()
                .filter_map(|r| r["full_name"].as_str().map(String::from)),
        );
        offset += page["results_shown"].as_u64().unwrap_or(0);
        if !page["results_has_more"].as_bool().unwrap_or(false) {
            break;
        }
    }
    assert!(
        seen.contains(
            &"Catalog.Новости.Template._СтандартныеЗначения_ЗдесьУказатьКодКанала".to_string()
        ),
        "неиспользуемый макет обязан остаться в наборе: {seen:?}"
    );
    assert!(!seen.contains(&"Catalog.Вакансии.Template.ПФ_MXL_Вакансия".to_string()));
}

/// A9: RU-форма `meta_type` нормализуется в каноническую на входе `execute`.
#[tokio::test]
async fn find_unreferenced_objects_meta_type_normalizes_russian() {
    let (_tmp, storage) = fresh_storage();
    mark_index_current(&storage).await;
    {
        let s = storage.get().await.unwrap();
        let conn = s.conn();
        for (fqn, mt, nm) in [
            ("Catalog.Нигде", "Catalog", "Нигде"),
            ("Subsystem.Продажи", "Subsystem", "Продажи"),
        ] {
            conn.execute(
                "INSERT INTO metadata_objects (repo, full_name, meta_type, name) \
                 VALUES ('default', ?1, ?2, ?3)",
                params![fqn, mt, nm],
            )
            .unwrap();
        }
        bsl_extension::schema::backfill_metadata_object_keys(conn).unwrap();
    }

    let res = run_tool(
        &FindUnreferencedObjectsTool,
        &storage,
        serde_json::json!({"repo": REPO, "meta_type": "Справочник"}),
    )
    .await;
    assert_eq!(res["results_total"].as_u64(), Some(1), "{res}");
    assert_eq!(
        res["results"][0]["full_name"].as_str(),
        Some("Catalog.Нигде")
    );
    assert!(
        res["not_addressable"].as_object().unwrap().is_empty(),
        "Subsystem вне фильтра Catalog не считается: {res}"
    );

    // Неизвестный тип нормализацией не портится — просто пустой результат.
    let res = run_tool(
        &FindUnreferencedObjectsTool,
        &storage,
        serde_json::json!({"repo": REPO, "meta_type": "НетТакогоТипа"}),
    )
    .await;
    assert_eq!(res["results_total"].as_u64(), Some(0), "{res}");
}

/// A9: `offset` за концом набора — пустая страница, `has_more=false`.
#[tokio::test]
async fn find_unreferenced_objects_offset_beyond_end_is_empty_page() {
    let (_tmp, storage) = fresh_storage();
    mark_index_current(&storage).await;
    {
        let s = storage.get().await.unwrap();
        let conn = s.conn();
        for i in 0..3 {
            conn.execute(
                "INSERT INTO metadata_objects (repo, full_name, meta_type, name) \
                 VALUES ('default', ?1, 'Catalog', ?2)",
                params![format!("Catalog.О{i}"), format!("О{i}")],
            )
            .unwrap();
        }
        bsl_extension::schema::backfill_metadata_object_keys(conn).unwrap();
    }

    let res = run_tool(
        &FindUnreferencedObjectsTool,
        &storage,
        serde_json::json!({"repo": REPO, "offset": 9999}),
    )
    .await;
    assert_eq!(res["results_total"].as_u64(), Some(3), "{res}");
    assert_eq!(res["results_shown"].as_u64(), Some(0), "{res}");
    assert_eq!(res["results_has_more"].as_bool(), Some(false), "{res}");
    assert!(res["results"].as_array().unwrap().is_empty(), "{res}");
}

/// A2: `channels_not_applied` присутствует и перечисляет непокрытые формы.
#[tokio::test]
async fn find_unreferenced_objects_reports_channels_not_applied() {
    let (_tmp, storage) = fresh_storage();
    mark_index_current(&storage).await;
    let res = run_tool(
        &FindUnreferencedObjectsTool,
        &storage,
        serde_json::json!({"repo": REPO}),
    )
    .await;
    let not_applied = res["channels_not_applied"]
        .as_array()
        .expect("channels_not_applied — массив");
    let forms: Vec<&str> = not_applied.iter().filter_map(|v| v.as_str()).collect();
    for form in [
        "template_call_via_receiver",
        "template_call_non_literal",
        "template_call_english",
        "template_call_multiline",
    ] {
        assert!(forms.contains(&form), "форма {form} не названа: {forms:?}");
    }
    assert!(res["channels_applied"]["template_calls_via_manager"].is_u64());
}

/// A1 (сквозной): исходник `Справочники.X.ПолучитьМакет("Y")` извлекается,
/// попадает в индекс с владельцем в `object_ref` и снимает `Catalog.X.Template.Y`.
#[tokio::test]
async fn find_unreferenced_objects_end_to_end_manager_template_call() {
    let (_tmp, storage) = fresh_storage();
    mark_index_current(&storage).await;
    let src =
        "Процедура П()\n\tСхема = Справочники.Вакансии.ПолучитьМакет(\"ПФ\");\nКонецПроцедуры";
    // Дополнительные каналы разбора (`extract_extra_usages`) — ровно то, что
    // кладёт в индекс индексатор для метод-формы макета.
    let usages = bsl_extension::code_usages::extract_extra_usages(src);
    assert!(
        usages
            .iter()
            .any(|u| u.usage_kind == "template_call_via_manager"
                && u.object_ref == "Catalog.Вакансии"),
        "{usages:?}"
    );
    {
        let s = storage.get().await.unwrap();
        let conn = s.conn();
        for (fqn, mt, nm) in [
            ("Catalog.Вакансии", "Catalog", "Вакансии"),
            ("Catalog.Вакансии.Template.ПФ", "Template", "ПФ"),
            ("Catalog.Вакансии.Template.Мёртвый", "Template", "Мёртвый"),
        ] {
            conn.execute(
                "INSERT INTO metadata_objects (repo, full_name, meta_type, name) \
                 VALUES ('default', ?1, ?2, ?3)",
                params![fqn, mt, nm],
            )
            .unwrap();
        }
        bsl_extension::schema::backfill_metadata_object_keys(conn).unwrap();
        for u in &usages {
            conn.execute(
                "INSERT INTO metadata_code_usages \
                 (repo, object_ref, object_ref_key, member_path, usage_kind, file_path, line) \
                 VALUES ('default', ?1, ?2, ?3, ?4, 'Catalogs/Вакансии/Ext/ManagerModule.bsl', ?5)",
                params![
                    u.object_ref,
                    u.object_ref_key,
                    u.member_path,
                    u.usage_kind,
                    u.line as i64
                ],
            )
            .unwrap();
        }
    }

    let res = run_tool(
        &FindUnreferencedObjectsTool,
        &storage,
        serde_json::json!({"repo": REPO, "meta_type": "Template"}),
    )
    .await;
    let names: Vec<&str> = res["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["full_name"].as_str())
        .collect();
    assert!(
        !names.contains(&"Catalog.Вакансии.Template.ПФ"),
        "метод-форма обязана снять ПФ: {names:?}"
    );
    assert!(
        names.contains(&"Catalog.Вакансии.Template.Мёртвый"),
        "неиспользуемый макет обязан остаться: {names:?}"
    );
    assert_eq!(
        res["channels_applied"]["template_calls_via_manager"].as_u64(),
        Some(1)
    );
}

/// B1 (сквозной): ИТОГОВЫЙ ответ инструмента (обёртка `{result,_meta}` целиком)
/// при любом бюджете не превышает запрошенный. Набор — 500 макетов-кандидатов,
/// страница заполняется целиком (несколько ходов на бюджет 8 000).
#[tokio::test]
async fn find_unreferenced_objects_response_fits_requested_budget() {
    let (_tmp, storage) = fresh_storage();
    mark_index_current(&storage).await;
    const N: usize = 500;
    {
        let s = storage.get().await.unwrap();
        let conn = s.conn();
        for i in 0..N {
            conn.execute(
                "INSERT INTO metadata_objects (repo, full_name, meta_type, name) \
                 VALUES ('default', ?1, 'Template', ?2)",
                params![
                    format!("Catalog.Объект{i:04}.Template.Макет{i:04}"),
                    format!("Макет{i:04}")
                ],
            )
            .unwrap();
        }
        bsl_extension::schema::backfill_metadata_object_keys(conn).unwrap();
    }

    let call = |args: Value| async {
        let ctx = ToolContext {
            repo: REPO,
            root_path: None,
            language: Some("bsl"),
            storage: &storage,
        };
        FindUnreferencedObjectsTool.execute(args, ctx).await
    };

    for budget in [8_000usize, 48_000] {
        let mut offset = 0usize;
        let mut seen = 0usize;
        let mut pages = 0usize;
        loop {
            pages += 1;
            assert!(pages <= N + 2, "зациклились на пагинации");
            let raw = call(serde_json::json!({
                "repo": REPO,
                "max_response_bytes": budget,
                "offset": offset,
            }))
            .await;
            let len = serde_json::to_string(&raw).unwrap().len();
            let res = &raw["result"];
            let shown = res["results"].as_array().unwrap().len();
            println!(
                "B1e2e budget={budget} offset={offset} -> len={len} shown={shown} has_more={}",
                res["results_has_more"]
            );
            assert!(
                len <= budget,
                "бюджет {budget}, offset {offset}: итоговый размер {len} превысил бюджет"
            );
            assert!(shown > 0, "страница не обнуляется, пока есть остаток");
            seen += shown;
            offset += shown;
            if !res["results_has_more"].as_bool().unwrap() {
                break;
            }
        }
        assert_eq!(seen, N, "бюджет {budget}: страницы покрывают весь набор");
    }

    // Кламп: запрос сверх серверного потолка зажимается, ответ ≤ потолка.
    let hard = code_index_core::mcp::cap::response_cap_hard();
    let raw = call(serde_json::json!({"repo": REPO, "max_response_bytes": 999_999})).await;
    let len = serde_json::to_string(&raw).unwrap().len();
    println!("B1e2e clamp(request=999999) -> len={len} hard={hard}");
    assert!(
        len <= hard,
        "при клампе итоговый размер {len} превысил потолок {hard}"
    );
    assert_eq!(raw["result"]["results_total"].as_u64(), Some(N as u64));
}

/// F2 (E-2): страж на уровне `execute` — утяжелённый обход обязан быть прерван
/// таймером `execute` и вернуть ОШИБКУ, а не висеть до конца (и не отдать
/// числа). Юнит-тест `blocking_query_is_interrupted_by_timer` проверяет только
/// сам механизм `spawn_blocking` + `sqlite3_interrupt`; проводка таймера в
/// `execute` проверяется здесь.
///
/// Обход делается заведомо дольше таймаута (`QUERY_TIMEOUT_SECS = 8`): у
/// `metadata_code_usages` снят индекс `idx_mcu_ref`, поэтому корреляционный
/// подзапрос обращений в `load_objects` вырождается в полный скан таблицы на
/// КАЖДЫЙ объект (200 000 строк × 20 000 объектов ≈ 4·10⁹ сравнений строк — это
/// десятки секунд, т.е. к моменту срабатывания таймера обход гарантированно
/// внутри одного длинного запроса, а не в промежутке между ними). Прерывание
/// доходит до `execute` ошибкой `OperationInterrupted` → ответ
/// `{error: "database error: …"}`.
///
/// Тест намеренно дорогой (≈8 с): это РЕАЛЬНЫЙ таймаут инструмента, а не
/// тестовый хук.
///
/// ВАЛИДНОСТЬ ЭТОГО ТЕСТА ДЕРЖИТСЯ НА ДВУХ ОПОРАХ (обе обязательны):
///
///   (а) РАНТАЙМ ТЕСТА — current-thread: у `#[tokio::test]` это дефолт, флаг
///       `flavor = "multi_thread"` здесь ставить НЕЛЬЗЯ. На multi-thread таймер
///       дёрнулся бы на другом воркере, даже если синхронный обход выполняется
///       прямо на async-воркере (без `spawn_blocking`), и страж перестал бы
///       ловить откат `spawn_blocking` — тест оставался бы зелёным на сломанном
///       коде. На current-thread заблокированный единственный поток не даёт
///       таймеру сработать, поэтому зелёный тест = таймер действительно
///       выполнился и действительно прервал обход.
///   (б) ИСКУССТВЕННО УТЯЖЕЛЁННЫЙ ОБХОД — снятый `idx_mcu_ref` (см. выше): без
///       него `load_objects` идёт по seek'ам и обход укладывается в таймаут, т.е.
///       тест ничего не проверял бы (нечего прерывать). Утяжеление — свойство
///       фикстуры теста, а не продакшн-плана.
///
/// ОРИЕНТИР ПЕРЕСМОТРА: если изменится план/скорость обхода `load_objects` (SQL
/// или индексы `metadata_code_usages`), значение `QUERY_TIMEOUT_SECS` или
/// константы `OBJECTS`/`USAGES`, тест надо пересчитать так, чтобы обход был
/// ЗАВЕДОМО дольше таймаута (сейчас — с большим запасом: ≈4·10⁹ сравнений строк
/// против 8 с); одновременно проверить, что нижняя граница `elapsed >= 7 с`
/// по-прежнему означает «сработал таймер», а не «завис».
#[tokio::test]
async fn find_unreferenced_objects_execute_interrupts_heavy_scan() {
    const OBJECTS: usize = 20_000;
    const USAGES: usize = 200_000;
    let (_tmp, storage) = fresh_storage();
    mark_index_current(&storage).await;
    {
        let s = storage.get().await.unwrap();
        let conn = s.conn();
        conn.execute("DROP INDEX IF EXISTS idx_mcu_ref", [])
            .unwrap();
        // Одной транзакцией: 220 000 отдельных автокоммитов — это минуты.
        conn.execute("BEGIN", []).unwrap();
        {
            let mut obj = conn
                .prepare(
                    "INSERT INTO metadata_objects (repo, full_name, meta_type, name) \
                     VALUES ('default', ?1, 'Catalog', ?2)",
                )
                .unwrap();
            for i in 0..OBJECTS {
                let name = format!("O{i:05}");
                obj.execute(params![format!("Catalog.{name}"), name])
                    .unwrap();
            }
        }
        {
            let mut usage = conn
                .prepare(
                    "INSERT INTO metadata_code_usages \
                     (repo, object_ref, object_ref_key, member_path, usage_kind, file_path, line) \
                     VALUES ('default', ?1, ?2, NULL, 'manager', \
                             'Catalogs/X/Ext/ManagerModule.bsl', 1)",
                )
                .unwrap();
            for i in 0..USAGES {
                let name = format!("O{:05}", i % OBJECTS);
                usage
                    .execute(params![
                        format!("Catalog.{name}"),
                        format!("catalog.{}", name.to_lowercase())
                    ])
                    .unwrap();
            }
        }
        bsl_extension::schema::backfill_metadata_object_keys(conn).unwrap();
        conn.execute("COMMIT", []).unwrap();
    }

    let t = Instant::now();
    let res = run_tool(
        &FindUnreferencedObjectsTool,
        &storage,
        serde_json::json!({"repo": REPO}),
    )
    .await;
    let elapsed = t.elapsed();
    println!("F2 execute+interrupt: elapsed={elapsed:?} res={res}");
    assert!(
        res["error"]
            .as_str()
            .unwrap_or_default()
            .contains("database error"),
        "утяжелённый обход обязан прерваться ошибкой базы, а не отдать числа: {res}"
    );
    assert!(
        res["results"].is_null(),
        "при прерывании чисел быть не должно: {res}"
    );
    // Нижняя граница — доказательство, что сработал таймер (≈8 с), а не «упало
    // сразу»; верхняя — что таймер именно прервал обход, а не завис.
    assert!(
        elapsed >= Duration::from_secs(7) && elapsed < Duration::from_secs(30),
        "таймер execute обязан прервать обход за ≈8 с: {elapsed:?}"
    );
}
