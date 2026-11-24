use rbs_api::Request;

pub(crate) fn validate_requests(requests: &[Request]) {
    let schemas = rbs_api::schema::builtin_schemas().unwrap();
    for request in requests {
        if let Some(pair) = schemas.get(request.method()) {
            let wire = serde_json::to_value(request).unwrap();
            let params = wire.get("params").unwrap();
            let validator = jsonschema::validator_for(&pair.input_schema).unwrap();
            assert!(
                validator.is_valid(params),
                "{}: {:?}",
                request.method(),
                validator.iter_errors(params).collect::<Vec<_>>()
            );
        }
    }
}
