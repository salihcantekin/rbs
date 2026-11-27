//! The default tool set (ADR-0196): small list, schemas on demand, same checks as
//! generated tools.

use super::*;

fn lazy_rig(scopes: &[Scope], irreversible: bool) -> (Rig, Calls) {
    let calls = Calls::default();
    let r = rig_with(vec![Some(api(&calls, scopes, irreversible))], ToolSet::Lazy);
    (r, calls)
}

fn text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

#[test]
fn the_default_list_is_small_and_has_no_per_method_tools() {
    let (mut r, _) = lazy_rig(&[Scope::Read, Scope::Control], false);
    let tools = r.tools();
    assert_eq!(
        tools,
        [
            "studio_summary",
            "api_index",
            "api_schema",
            "api_read",
            "api_edit"
        ]
    );
    let list = r.send(r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#);
    let size = list[0]["result"].to_string().len();
    assert!(size < 4_000, "tools/list is {size} bytes");
    let (mut r, _) = lazy_rig(&[Scope::Read, Scope::Control], true);
    assert!(r.tools().contains(&"api_live".to_owned()));
}

#[test]
fn the_index_lists_only_what_the_session_may_call() {
    let (mut r, _) = lazy_rig(&[Scope::Read, Scope::Control], false);
    let (result, _) = r.call("api_index", json!({}));
    assert_eq!(result["isError"], false);
    let names: Vec<&str> = result["structuredContent"]["namespaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"scene"));
    for hidden in ["tokens", "session", "events"] {
        assert!(!names.contains(&hidden), "{hidden}");
    }
    let (result, _) = r.call("api_index", json!({"namespace": "scene"}));
    let methods = result["structuredContent"]["methods"].as_array().unwrap();
    let list = methods
        .iter()
        .find(|m| m["method"] == "scene.list")
        .unwrap();
    assert_eq!(list["tool"], "api_read");
    assert!(!list["description"].as_str().unwrap().is_empty());
    assert!(
        methods
            .iter()
            .all(|m| m["method"] != "scene.remove" || m["tool"] != "api_live")
    );
    // A hidden namespace and a missing one answer alike.
    let (hidden, _) = r.call("api_index", json!({"namespace": "tokens"}));
    let (missing, _) = r.call("api_index", json!({"namespace": "nope"}));
    assert_eq!(hidden["isError"], true);
    assert_eq!(
        text(&hidden).replace("tokens", "X"),
        text(&missing).replace("nope", "X")
    );
}

#[test]
fn schemas_are_fetched_for_the_named_methods_only() {
    let (mut r, calls) = lazy_rig(&[Scope::Read, Scope::Control], false);
    let before = calls.lock().unwrap().len();
    let (result, _) = r.call(
        "api_schema",
        json!({"methods": ["scene.add", "scene.list"]}),
    );
    assert_eq!(result["isError"], false, "{result}");
    let methods = &result["structuredContent"]["methods"];
    assert_eq!(methods["scene.add"]["tool"], "api_edit");
    assert_eq!(methods["scene.add"]["input_schema"]["type"], "object");
    assert!(methods["scene.list"]["output_schema"].is_object());
    let all = calls.lock().unwrap();
    let described = &all[before..];
    assert_eq!(described.len(), 1);
    assert_eq!(described[0].0, "api.describe");
    assert_eq!(
        described[0].1,
        json!({"detail": "schemas", "methods": ["scene.add", "scene.list"]})
    );
    drop(all);
    // Cached for the connection: no second request.
    r.call("api_schema", json!({"methods": ["scene.add"]}));
    assert_eq!(calls.lock().unwrap().len(), before + 1);
}

#[test]
fn schema_requests_are_bounded_and_hide_hidden_methods() {
    let (mut r, calls) = lazy_rig(&[Scope::Read, Scope::Control], false);
    let many: Vec<String> = (0..17).map(|i| format!("scene.m{i}")).collect();
    let (result, _) = r.call("api_schema", json!({"methods": many}));
    assert_eq!(result["isError"], true);
    let (result, _) = r.call("api_schema", json!({"methods": []}));
    assert_eq!(result["isError"], true);
    let (unknown, _) = r.call("api_schema", json!({"methods": ["scene.nope"]}));
    let n = calls.lock().unwrap().len();
    for hidden in ["tokens.list", "edit.undo", "record.start"] {
        let (result, _) = r.call("api_schema", json!({"methods": [hidden]}));
        assert_eq!(result["isError"], true, "{hidden}");
        assert_eq!(
            text(&result).replace(hidden, "X"),
            text(&unknown).replace("scene.nope", "X"),
            "{hidden}"
        );
    }
    assert_eq!(calls.lock().unwrap().len(), n, "nothing was sent to RBS");
}

#[test]
fn a_method_is_called_through_the_tool_of_its_effect() {
    let (mut r, calls) = lazy_rig(&[Scope::Read, Scope::Control], false);
    let (result, _) = r.call("api_read", json!({"method": "scene.list"}));
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(calls.lock().unwrap().last().unwrap().0, "scene.list");
    let (result, _) = r.call(
        "api_edit",
        json!({"method": "scene.rename", "params": {"scene": 2, "name": "Intro"}}),
    );
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(
        calls.lock().unwrap().last().cloned().unwrap(),
        ("scene.rename".into(), json!({"scene": 2, "name": "Intro"}))
    );
    let (wrong, _) = r.call("api_edit", json!({"method": "scene.list"}));
    assert_eq!(wrong["isError"], true);
    assert!(text(&wrong).contains("api_read"), "{}", text(&wrong));
    let (wrong, _) = r.call("api_read", json!({"method": "scene.rename"}));
    assert!(text(&wrong).contains("api_edit"), "{}", text(&wrong));
}

#[test]
fn hidden_methods_are_unknown_through_every_call_tool() {
    let (mut r, calls) = lazy_rig(&[Scope::Read, Scope::Control], false);
    let n = calls.lock().unwrap().len();
    let (unknown, _) = r.call("api_read", json!({"method": "scene.nope"}));
    for tool in ["api_read", "api_edit", "api_live"] {
        // `api_live` is not listed without the permission, so it is not a tool at all.
        for hidden in [
            "tokens.list",
            "session.hello",
            "edit.undo",
            "record.start",
            "plugin.list",
        ] {
            let (result, _) = r.call(tool, json!({"method": hidden}));
            assert_eq!(result["isError"], true, "{tool} {hidden}");
            if tool != "api_live" {
                assert_eq!(
                    text(&result).replace(hidden, "X"),
                    text(&unknown)
                        .replace("scene.nope", "X")
                        .replace("api_read", tool),
                    "{tool} {hidden}"
                );
            }
        }
    }
    assert_eq!(calls.lock().unwrap().len(), n, "nothing was sent to RBS");
}

#[test]
fn live_methods_go_through_api_live_with_the_permission() {
    let (mut r, calls) = lazy_rig(&[Scope::Read, Scope::Control], true);
    let (wrong, _) = r.call("api_edit", json!({"method": "record.start"}));
    assert!(text(&wrong).contains("api_live"), "{}", text(&wrong));
    let (result, _) = r.call("api_live", json!({"method": "record.start"}));
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(calls.lock().unwrap().last().unwrap().0, "record.start");
    let (hidden, _) = r.call("api_live", json!({"method": "tokens.create"}));
    assert_eq!(hidden["isError"], true);
    assert!(!text(&hidden).contains("api_live"), "{}", text(&hidden));
}

#[test]
fn the_fixed_tools_are_not_offered_while_not_connected() {
    let mut r = rig_with(vec![None], ToolSet::Lazy);
    assert_eq!(r.tools(), ["studio_summary"]);
    let (result, _) = r.call("api_index", json!({}));
    assert_eq!(result["isError"], true);
}

#[test]
fn tools_flag_values_parse() {
    assert_eq!(ToolSet::parse("all"), ToolSet::All);
    assert_eq!(
        ToolSet::parse("scene, source"),
        ToolSet::Namespaces(vec!["scene".into(), "source".into()])
    );
    assert_eq!(ToolSet::parse(""), ToolSet::Lazy);
}
