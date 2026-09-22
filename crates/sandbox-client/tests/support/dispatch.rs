#![allow(clippy::unwrap_used)]
use base64::{Engine, engine::general_purpose::STANDARD};
use sandbox_client::{Client, Error, RangeChunk, requests::*};
use serde_json::{Value, json};
fn chunk(c: RangeChunk) -> Value {
    let mut v = serde_json::to_value(&c).unwrap();
    v["data_base64"] = STANDARD.encode(&c.bytes).into();
    v
}
pub(crate) async fn dispatch(client: &Client, case: &Value) -> Result<Value, Error> {
    let args = &case["args"];
    match case["action"].as_str().unwrap() {
        "createSandbox" => {
            let body = serde_json::from_value(args["body"].clone()).map_err(|_| Error::Request)?;
            Ok(serde_json::to_value(
                client
                    .create_sandbox(CreateSandbox {
                        idempotency_key: args["idempotency_key"].as_str().unwrap(),
                        body: &body,
                    })
                    .await?,
            )
            .unwrap())
        }
        "listSandboxes" => Ok(serde_json::to_value(
            client
                .list_sandboxes(ListSandboxes {
                    limit: args["limit"].as_u64(),
                    cursor: args["cursor"].as_str(),
                })
                .await?,
        )
        .unwrap()),
        "getSandbox" => Ok(serde_json::to_value(
            client
                .get_sandbox(GetSandbox {
                    sandbox_id: args["sandbox_id"].as_str().unwrap(),
                })
                .await?,
        )
        .unwrap()),
        "executeCommand" => {
            let body = serde_json::from_value(args["body"].clone()).map_err(|_| Error::Request)?;
            Ok(serde_json::to_value(
                client
                    .execute_command(ExecuteCommand {
                        sandbox_id: args["sandbox_id"].as_str().unwrap(),
                        idempotency_key: args["idempotency_key"].as_str().unwrap(),
                        body: &body,
                    })
                    .await?,
            )
            .unwrap())
        }
        "destroySandbox" => {
            let body = serde_json::from_value(args["body"].clone()).map_err(|_| Error::Request)?;
            Ok(serde_json::to_value(
                client
                    .destroy_sandbox(DestroySandbox {
                        sandbox_id: args["sandbox_id"].as_str().unwrap(),
                        idempotency_key: args["idempotency_key"].as_str().unwrap(),
                        body: &body,
                    })
                    .await?,
            )
            .unwrap())
        }
        "listOperations" => Ok(serde_json::to_value(
            client
                .list_operations(ListOperations {
                    limit: args["limit"].as_u64(),
                    cursor: args["cursor"].as_str(),
                    sandbox_id: args["sandbox_id"].as_str(),
                })
                .await?,
        )
        .unwrap()),
        "getOperation" => Ok(serde_json::to_value(
            client
                .get_operation(GetOperation {
                    operation_id: args["operation_id"].as_str().unwrap(),
                })
                .await?,
        )
        .unwrap()),
        "cancelCommand" => {
            let body = serde_json::from_value(args["body"].clone()).map_err(|_| Error::Request)?;
            Ok(serde_json::to_value(
                client
                    .cancel_command(CancelCommand {
                        operation_id: args["operation_id"].as_str().unwrap(),
                        idempotency_key: args["idempotency_key"].as_str().unwrap(),
                        body: &body,
                    })
                    .await?,
            )
            .unwrap())
        }
        "readOutput" => Ok(chunk(
            client
                .read_output(ReadOutput {
                    operation_id: args["operation_id"].as_str().unwrap(),
                    output_name: args["output_name"].as_str().unwrap(),
                    offset: args["offset"].as_u64(),
                    limit: args["limit"].as_u64(),
                })
                .await?,
        )),
        "streamOutput" => {
            let mut stream = client
                .stream_output(StreamOutput {
                    operation_id: args["operation_id"].as_str().unwrap(),
                    cursor: args["cursor"].as_str(),
                    last_event_id: args["last_event_id"].as_str(),
                })
                .await?;
            let mut events = Vec::new();
            while let Some(event) = stream.next().await? {
                events.push(serde_json::to_value(event).unwrap());
            }
            Ok(json!(events))
        }
        "uploadFile" => {
            let body = STANDARD
                .decode(args["body_base64"].as_str().unwrap())
                .unwrap();
            Ok(serde_json::to_value(
                client
                    .upload_file(UploadFile {
                        sandbox_id: args["sandbox_id"].as_str().unwrap(),
                        idempotency_key: args["idempotency_key"].as_str().unwrap(),
                        path: args["path"].as_str().unwrap(),
                        x_file_size: args["x_file_size"].as_u64().unwrap(),
                        x_file_sha256: args["x_file_sha256"].as_str().unwrap(),
                        x_file_mode: args["x_file_mode"].as_str(),
                        body: &body,
                    })
                    .await?,
            )
            .unwrap())
        }
        "captureFile" => {
            let body = serde_json::from_value(args["body"].clone()).map_err(|_| Error::Request)?;
            Ok(serde_json::to_value(
                client
                    .capture_file(CaptureFile {
                        sandbox_id: args["sandbox_id"].as_str().unwrap(),
                        body: &body,
                    })
                    .await?,
            )
            .unwrap())
        }
        "readCapturedFile" => Ok(chunk(
            client
                .read_captured_file(ReadCapturedFile {
                    sandbox_id: args["sandbox_id"].as_str().unwrap(),
                    x_file_capture: args["x_file_capture"].as_str().unwrap(),
                    offset: args["offset"].as_u64(),
                    limit: args["limit"].as_u64(),
                })
                .await?,
        )),
        "releaseFileCapture" => Ok(serde_json::to_value(
            client
                .release_file_capture(ReleaseFileCapture {
                    sandbox_id: args["sandbox_id"].as_str().unwrap(),
                    x_file_capture: args["x_file_capture"].as_str().unwrap(),
                })
                .await?,
        )
        .unwrap()),
        "download" => Ok(serde_json::to_value(
            client
                .download(
                    args["sandbox_id"].as_str().unwrap(),
                    args["path"].as_str().unwrap(),
                    std::path::Path::new(args["destination"].as_str().unwrap()),
                )
                .await?,
        )
        .unwrap()),
        "wait" => Ok(serde_json::to_value(
            client
                .wait(
                    args["operation_id"].as_str().unwrap(),
                    std::time::Duration::from_secs_f64(args["seconds"].as_f64().unwrap()),
                )
                .await?,
        )
        .unwrap()),
        _ => panic!("unknown conformance action"),
    }
}
