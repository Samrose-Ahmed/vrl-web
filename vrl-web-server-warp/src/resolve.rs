use ::value::{Secrets, Value};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::convert::Infallible;
use vector_common::TimeZone;
use vrl::{diagnostic::Formatter, state, value, Program, Runtime, TargetValueRef};
use warp::{reply::json, Reply};

use anyhow::{anyhow, Result};
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use lru::LruCache;
use std::sync::Arc;
use std::sync::Mutex;
use std::{cell::RefCell, time::Instant};
use crate::bit_and::BitwiseAnd;

thread_local! {
    pub static RUNTIME: RefCell<Runtime> = RefCell::new(Runtime::new(state::Runtime::default()));
}

pub fn custom_vrl_functions() -> Vec<Box<dyn vrl::Function>> {
    vec![
        Box::new(BitwiseAnd) as _,
    ]
}

// The VRL program plus (optional) event plus (optional) time zone
#[derive(Deserialize, Serialize)]
pub(crate) struct Input {
    program: String,
    event: Option<Value>,
    tz: Option<String>,
}

// An enum for the result of a VRL resolution operation
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Outcome {
    Success { output: Value, result: Value },
    Error(String),
}

// The VRL resolution logic
fn resolve(input: Input) -> Outcome {
    lazy_static! {
        static ref CACHE: Arc<Mutex<LruCache<String, Result<Program, String>>>> = Arc::new(
            Mutex::new(LruCache::new(std::num::NonZeroUsize::new(400).unwrap()))
        );
    };

    let mut value: Value = input.event.unwrap_or(value!({}));
    let program = input.program.as_str();

    let mut cache_ref = CACHE.lock().unwrap();

    let stored_result = (*cache_ref).get(program);

    let mut functions = vrl_stdlib::all();
    functions.append(&mut custom_vrl_functions());

    let start = Instant::now();
    let compiled = match stored_result {
        Some(compiled) => match compiled {
            Ok(compiled) => Ok(compiled),
            Err(e) => {
                return Outcome::Error(e.clone());
            }
        },
        None => match vrl::compile(program, &functions) {
            Ok(result) => {
                debug!(
                    "Compiled a vrl program ({}), took {:?}",
                    program
                        .lines()
                        .into_iter()
                        .skip(1)
                        .next()
                        .unwrap_or("expansion"),
                    start.elapsed()
                );
                (*cache_ref).put(program.to_string(), Ok(result.program));
                if result.warnings.len() > 0 {
                    warn!("{:?}", result.warnings);
                }
                match (*cache_ref).get(program) {
                    Some(compiled) => match compiled {
                        Ok(compiled) => Ok(compiled),
                        Err(e) => {
                            return Outcome::Error(e.clone());
                        }
                    },
                    None => unreachable!(),
                }
            }
            Err(diagnostics) => {
                let msg = Formatter::new(&program, diagnostics).to_string();
                (*cache_ref).put(program.to_string(), Err(msg.clone()));
                Err(anyhow!(msg))
            }
        },
    };

    if compiled.is_err() {
        return Outcome::Error(compiled.err().unwrap().to_string());
    }
    let compiled = compiled.unwrap();

    let mut metadata = ::value::Value::Object(BTreeMap::new());
    let mut secrets = ::value::Secrets::new();
    let mut target = TargetValueRef {
        value: &mut value,
        metadata: &mut metadata,
        secrets: &mut secrets,
    };

    let time_zone_str = Some("tt".to_string()).unwrap_or_default();

    let time_zone = match TimeZone::parse(&time_zone_str) {
        Some(tz) => tz,
        None => TimeZone::Local,
    };

    let result = RUNTIME.with(|r| {
        let mut runtime = r.borrow_mut();

        match (*runtime).resolve(&mut target, &compiled, &time_zone) {
            Ok(result) => Ok(result),
            Err(err) => Err(err.to_string()),
        }
    });

    let res = match result {
        Ok(result) => Outcome::Success {
            output: result,
            result: value,
        },
        Err(err) => Outcome::Error(err),
    };

    res
}

// The VRL resolution logic as an HTTP handler
pub(crate) async fn resolve_vrl_input(input: Input) -> Result<impl Reply, Infallible> {
    let outcome = resolve(input);
    Ok(json(&outcome))
}

#[cfg(test)]
mod tests {
    // Just a small handful of tests here that pretty much only test the HTTP
    // plumbing. The assumption, of course, is that VRL itself has its ducks in
    // a row.

    use super::{Input, Outcome};
    use crate::server::router;
    use http::StatusCode;
    use serde_json::{json, Value};
    use vrl::{prelude::Bytes, value};

    fn assert_outcome_matches_expected(outcome: Outcome, body: &Bytes) {
        let s: String = serde_json::to_string(&outcome).unwrap();
        let b: Bytes = Bytes::from(s);

        assert_eq!(body, &b);
    }

    #[tokio::test]
    async fn test_successful_resolution() {
        let test_cases: Vec<(Input, Outcome)> = vec![
            (
                Input {
                    program: r#".foo = "bar""#.to_owned(),
                    event: None,
                    tz: None,
                },
                Outcome::Success {
                    result: value!({"foo": "bar"}),
                    output: value!("bar"),
                },
            ),
            (
                Input {
                    program: r#".tags.environment = "production"; del(.delete_me)"#.to_owned(),
                    event: Some(value!({"delete_me": "bye bye"})),
                    tz: None,
                },
                Outcome::Success {
                    result: value!({"tags": {"environment": "production"}}),
                    output: value!("bye bye"),
                },
            ),
        ];

        for tc in test_cases {
            let res = warp::test::request()
                .method("POST")
                .path("/resolve")
                .json(&tc.0)
                .reply(&router())
                .await;
            assert_eq!(res.status(), StatusCode::OK);
            assert_outcome_matches_expected(tc.1, res.body());
        }
    }

    #[tokio::test]
    async fn test_failures() {
        let test_cases: Vec<Value> = vec![
            // No program or event
            json!({"this": "won't work"}),
            // No program
            json!({"event": {"tags": {"environment": "staging"}}}),
        ];

        for tc in test_cases {
            let res = warp::test::request()
                .method("POST")
                .path("/resolve")
                .body(tc.to_string())
                .reply(&router())
                .await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        }
    }
}
