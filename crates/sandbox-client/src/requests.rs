//! Generated from api/openapi.json. Do not edit.
use crate::{Client, Error, EventStream, RangeChunk, models::*};
pub(crate) fn known_problem_code(code: &str) -> Option<&'static str> {
    match code {
        "bad_request" => Some("bad_request"),
        "payload_too_large" => Some("payload_too_large"),
        "unauthenticated" => Some("unauthenticated"),
        "forbidden" => Some("forbidden"),
        "image_not_allowed" => Some("image_not_allowed"),
        "not_found" => Some("not_found"),
        "gone" => Some("gone"),
        "response_expired" => Some("response_expired"),
        "output_not_ready" => Some("output_not_ready"),
        "output_expired" => Some("output_expired"),
        "output_missing" => Some("output_missing"),
        "output_corrupt" => Some("output_corrupt"),
        "output_range_invalid" => Some("output_range_invalid"),
        "file_capture_missing" => Some("file_capture_missing"),
        "file_response_invalid" => Some("file_response_invalid"),
        "file_range_invalid" => Some("file_range_invalid"),
        "command_in_progress" => Some("command_in_progress"),
        "execution_capacity_exhausted" => Some("execution_capacity_exhausted"),
        "conflict" => Some("conflict"),
        "unavailable" => Some("unavailable"),
        "internal" => Some("internal"),
        _ => None,
    }
}
pub struct CreateSandbox<'a> {
    pub idempotency_key: &'a str,
    pub body: &'a CreateRequest,
}
impl std::fmt::Debug for CreateSandbox<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateSandbox").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn create_sandbox(&self, args: CreateSandbox<'_>) -> Result<AdmittedResponse, Error> {
        let mut request = self.request(reqwest::Method::POST, &["v1", "sandboxes"])?;
        request = self.header(request, "Idempotency-Key", args.idempotency_key)?;
        request = self.json_body(request, args.body)?;
        self.json(request, 202).await
    }
}
pub struct ListSandboxes<'a> {
    pub limit: Option<u64>,
    pub cursor: Option<&'a str>,
}
impl std::fmt::Debug for ListSandboxes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListSandboxes").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn list_sandboxes(&self, args: ListSandboxes<'_>) -> Result<SandboxList, Error> {
        let mut request = self.request(reqwest::Method::GET, &["v1", "sandboxes"])?;
        if let Some(value) = args.limit {
            request = self.query(request, "limit", &value.to_string())?;
        }
        if let Some(value) = args.cursor {
            request = self.query(request, "cursor", value)?;
        }
        self.json(request, 200).await
    }
}
pub struct GetSandbox<'a> {
    pub sandbox_id: &'a str,
}
impl std::fmt::Debug for GetSandbox<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GetSandbox").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn get_sandbox(&self, args: GetSandbox<'_>) -> Result<SandboxBody, Error> {
        let request = self.request(reqwest::Method::GET, &["v1", "sandboxes", args.sandbox_id])?;
        self.json(request, 200).await
    }
}
pub struct ExecuteCommand<'a> {
    pub sandbox_id: &'a str,
    pub idempotency_key: &'a str,
    pub body: &'a CommandInput,
}
impl std::fmt::Debug for ExecuteCommand<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecuteCommand").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn execute_command(
        &self,
        args: ExecuteCommand<'_>,
    ) -> Result<AdmittedResponse, Error> {
        let mut request = self.request(
            reqwest::Method::POST,
            &["v1", "sandboxes", args.sandbox_id, "execute"],
        )?;
        request = self.header(request, "Idempotency-Key", args.idempotency_key)?;
        request = self.json_body(request, args.body)?;
        self.json(request, 202).await
    }
}
pub struct DestroySandbox<'a> {
    pub sandbox_id: &'a str,
    pub idempotency_key: &'a str,
    pub body: &'a DestroyRequest,
}
impl std::fmt::Debug for DestroySandbox<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DestroySandbox").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn destroy_sandbox(
        &self,
        args: DestroySandbox<'_>,
    ) -> Result<AdmittedResponse, Error> {
        let mut request = self.request(
            reqwest::Method::POST,
            &["v1", "sandboxes", args.sandbox_id, "destroy"],
        )?;
        request = self.header(request, "Idempotency-Key", args.idempotency_key)?;
        request = self.json_body(request, args.body)?;
        self.json(request, 202).await
    }
}
pub struct ListOperations<'a> {
    pub limit: Option<u64>,
    pub cursor: Option<&'a str>,
    pub sandbox_id: Option<&'a str>,
}
impl std::fmt::Debug for ListOperations<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListOperations").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn list_operations(&self, args: ListOperations<'_>) -> Result<OperationList, Error> {
        let mut request = self.request(reqwest::Method::GET, &["v1", "operations"])?;
        if let Some(value) = args.limit {
            request = self.query(request, "limit", &value.to_string())?;
        }
        if let Some(value) = args.cursor {
            request = self.query(request, "cursor", value)?;
        }
        if let Some(value) = args.sandbox_id {
            request = self.query(request, "sandbox_id", value)?;
        }
        self.json(request, 200).await
    }
}
pub struct GetOperation<'a> {
    pub operation_id: &'a str,
}
impl std::fmt::Debug for GetOperation<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GetOperation").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn get_operation(&self, args: GetOperation<'_>) -> Result<OperationBody, Error> {
        let request = self.request(
            reqwest::Method::GET,
            &["v1", "operations", args.operation_id],
        )?;
        self.json(request, 200).await
    }
}
pub struct CancelCommand<'a> {
    pub operation_id: &'a str,
    pub idempotency_key: &'a str,
    pub body: &'a CancelRequest,
}
impl std::fmt::Debug for CancelCommand<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelCommand").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn cancel_command(&self, args: CancelCommand<'_>) -> Result<AdmittedResponse, Error> {
        let mut request = self.request(
            reqwest::Method::POST,
            &["v1", "operations", args.operation_id, "cancel"],
        )?;
        request = self.header(request, "Idempotency-Key", args.idempotency_key)?;
        request = self.json_body(request, args.body)?;
        self.json(request, 202).await
    }
}
pub struct ReadOutput<'a> {
    pub operation_id: &'a str,
    pub output_name: &'a str,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}
impl std::fmt::Debug for ReadOutput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadOutput").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn read_output(&self, args: ReadOutput<'_>) -> Result<RangeChunk, Error> {
        let mut request = self.request(
            reqwest::Method::GET,
            &[
                "v1",
                "operations",
                args.operation_id,
                "outputs",
                args.output_name,
            ],
        )?;
        if let Some(value) = args.offset {
            request = self.query(request, "offset", &value.to_string())?;
        }
        if let Some(value) = args.limit {
            request = self.query(request, "limit", &value.to_string())?;
        }
        self.binary(
            request,
            200,
            "output",
            args.offset.unwrap_or(0),
            args.limit.unwrap_or(32768),
        )
        .await
    }
}
pub struct StreamOutput<'a> {
    pub operation_id: &'a str,
    pub cursor: Option<&'a str>,
    pub last_event_id: Option<&'a str>,
}
impl std::fmt::Debug for StreamOutput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamOutput").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn stream_output(&self, args: StreamOutput<'_>) -> Result<EventStream, Error> {
        let mut request = self.request(
            reqwest::Method::GET,
            &["v1", "operations", args.operation_id, "stream"],
        )?;
        if let Some(value) = args.cursor {
            request = self.query(request, "cursor", value)?;
        }
        if let Some(value) = args.last_event_id {
            request = self.header(request, "Last-Event-ID", value)?;
        }
        self.events(request, 200).await
    }
}
pub struct UploadFile<'a> {
    pub sandbox_id: &'a str,
    pub idempotency_key: &'a str,
    pub path: &'a str,
    pub x_file_size: u64,
    pub x_file_sha256: &'a str,
    pub x_file_mode: Option<&'a str>,
    pub body: &'a [u8],
}
impl std::fmt::Debug for UploadFile<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadFile").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn upload_file(&self, args: UploadFile<'_>) -> Result<AdmittedResponse, Error> {
        let mut request = self.request(
            reqwest::Method::PUT,
            &["v1", "sandboxes", args.sandbox_id, "files"],
        )?;
        request = self.header(request, "Idempotency-Key", args.idempotency_key)?;
        request = self.query(request, "path", args.path)?;
        request = self.header(request, "X-File-Size", &args.x_file_size.to_string())?;
        request = self.header(request, "X-File-SHA256", args.x_file_sha256)?;
        if let Some(value) = args.x_file_mode {
            request = self.header(request, "X-File-Mode", value)?;
        }
        request = self.binary_body(request, args.body)?;
        self.json(request, 202).await
    }
}
pub struct CaptureFile<'a> {
    pub sandbox_id: &'a str,
    pub body: &'a FileCaptureRequest,
}
impl std::fmt::Debug for CaptureFile<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureFile").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn capture_file(&self, args: CaptureFile<'_>) -> Result<FileCaptureResponse, Error> {
        let mut request = self.request(
            reqwest::Method::POST,
            &["v1", "sandboxes", args.sandbox_id, "files", "captures"],
        )?;
        request = self.json_body(request, args.body)?;
        self.json(request, 201).await
    }
}
pub struct ReadCapturedFile<'a> {
    pub sandbox_id: &'a str,
    pub x_file_capture: &'a str,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}
impl std::fmt::Debug for ReadCapturedFile<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadCapturedFile").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn read_captured_file(
        &self,
        args: ReadCapturedFile<'_>,
    ) -> Result<RangeChunk, Error> {
        let mut request = self.request(
            reqwest::Method::GET,
            &["v1", "sandboxes", args.sandbox_id, "files", "captures"],
        )?;
        request = self.header(request, "X-File-Capture", args.x_file_capture)?;
        if let Some(value) = args.offset {
            request = self.query(request, "offset", &value.to_string())?;
        }
        if let Some(value) = args.limit {
            request = self.query(request, "limit", &value.to_string())?;
        }
        self.binary(
            request,
            200,
            "file",
            args.offset.unwrap_or(0),
            args.limit.unwrap_or(32768),
        )
        .await
    }
}
pub struct ReleaseFileCapture<'a> {
    pub sandbox_id: &'a str,
    pub x_file_capture: &'a str,
}
impl std::fmt::Debug for ReleaseFileCapture<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReleaseFileCapture").finish_non_exhaustive()
    }
}
impl Client {
    pub async fn release_file_capture(&self, args: ReleaseFileCapture<'_>) -> Result<(), Error> {
        let mut request = self.request(
            reqwest::Method::DELETE,
            &["v1", "sandboxes", args.sandbox_id, "files", "captures"],
        )?;
        request = self.header(request, "X-File-Capture", args.x_file_capture)?;
        self.empty(request, 204).await
    }
}
