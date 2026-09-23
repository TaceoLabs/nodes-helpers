use taceo_serde_partial_default::SerdePartialDefault;

fn default_retries() -> u32 {
    3
}

fn default_timeout_secs() -> u64 {
    30
}

#[derive(Debug, PartialEq, serde::Deserialize, SerdePartialDefault)]
struct Config {
    name: String,
    #[serde(default = "default_retries")]
    retries: u32,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
}

#[test]
fn partial_default_fills_marked_fields() {
    let config = Config::partial_default("svc".to_string());
    assert_eq!(
        config,
        Config {
            name: "svc".to_string(),
            retries: 3,
            timeout_secs: 30,
        }
    );
}

#[test]
fn struct_update_overrides_individual_defaults() {
    let config = Config {
        retries: 5,
        ..Config::partial_default("svc".to_string())
    };
    assert_eq!(
        config,
        Config {
            name: "svc".to_string(),
            retries: 5,
            timeout_secs: 30,
        }
    );
}

#[derive(Debug, PartialEq, serde::Deserialize, SerdePartialDefault)]
struct AllRequired {
    a: u32,
    b: u32,
}

#[test]
fn all_required_fields_take_all_as_params() {
    let value = AllRequired::partial_default(1, 2);
    assert_eq!(value, AllRequired { a: 1, b: 2 });
}

#[derive(Debug, PartialEq, serde::Deserialize, SerdePartialDefault)]
struct AllDefaulted {
    #[serde(default = "default_retries")]
    retries: u32,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
}

#[test]
fn all_defaulted_fields_generate_default_impl() {
    let value = AllDefaulted::default();
    assert_eq!(
        value,
        AllDefaulted {
            retries: 3,
            timeout_secs: 30,
        }
    );
}

#[derive(Debug, PartialEq, serde::Deserialize, SerdePartialDefault)]
struct BareDefault {
    #[serde(default)]
    retries: u32,
}

#[test]
fn bare_serde_default_uses_default_trait() {
    let value = BareDefault::default();
    assert_eq!(value, BareDefault { retries: 0 });
}
