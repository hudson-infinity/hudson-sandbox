//! Guest-only vsock listener. mTLS is mandatory even after host-CID validation.
use crate::{file_service::FileService, runner::Runner};
use anyhow::{Context as _, Result, ensure};
use sandbox_protocol::{
    OperationId, guest as w, guest_model as m,
    guest_wire::{self as wire, ServerTls},
};
use std::sync::Arc;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::Semaphore,
    task::JoinSet,
};

pub async fn serve_connection<S: AsyncRead + AsyncWrite + Unpin>(
    runner: &Runner,
    tls: &ServerTls,
    stream: S,
) -> Result<()> {
    serve_connection_with_files(runner, None, tls, stream).await
}
pub async fn serve_connection_with_files<S: AsyncRead + AsyncWrite + Unpin>(
    runner: &Runner,
    files: Option<&FileService>,
    tls: &ServerTls,
    stream: S,
) -> Result<()> {
    tokio::time::timeout(wire::CONNECTION_TIMEOUT, async {
        let mut stream = tls.accept(stream).await?;
        let request: w::Request = wire::read_frame(&mut stream).await?;
        let response = dispatch(runner, files, request).await?;
        wire::write_frame(&mut stream, &response).await
    })
    .await
    .context("guest connection deadline")?
}
async fn dispatch(
    runner: &Runner,
    files: Option<&FileService>,
    request: w::Request,
) -> Result<w::Response> {
    use w::{request::Action as A, response::Result as R};
    ensure!(
        request.version == wire::VERSION,
        "unsupported guest protocol"
    );
    request.request_id.parse::<OperationId>()?;
    let context: m::Context = request.context.context("missing context")?.try_into()?;
    let action = request.action.context("missing action")?;
    let actual = runner.context();
    ensure!(
        context.allocation_id == actual.allocation_id && context.generation == actual.generation,
        "wrong allocation context"
    );
    ensure!(
        context.boot_id == actual.boot_id
            || (matches!(action, A::Hello(_)) && context.boot_id.is_empty()),
        "wrong boot identity"
    );
    if let Some(files) = files {
        ensure!(files.context() == actual, "file service context mismatch");
    }
    let result = match action {
        A::Hello(_) => R::Hello(w::Hello {}),
        A::Execute(value) => match m::Execute::try_from(value) {
            Ok(value) => match runner.start(value).await {
                Ok(receipt) => R::Receipt((&receipt).into()),
                Err(_) => R::Error(w::Error {
                    code: w::ErrorCode::Uncertain as i32,
                }),
            },
            Err(_) => R::Error(w::Error {
                code: w::ErrorCode::Invalid as i32,
            }),
        },
        A::Inspect(value) => match value.operation_id.parse() {
            Ok(id) => match runner.inspect(id).await {
                Some(receipt) => R::Receipt((&receipt).into()),
                None => R::Error(w::Error {
                    code: w::ErrorCode::NotFound as i32,
                }),
            },
            Err(_) => R::Error(w::Error {
                code: w::ErrorCode::Invalid as i32,
            }),
        },
        A::Cancel(value) => match value.operation_id.parse() {
            Ok(id) => match runner.cancel(id).await {
                Ok(receipt) => R::Receipt((&receipt).into()),
                Err(_) => R::Error(w::Error {
                    code: w::ErrorCode::Uncertain as i32,
                }),
            },
            Err(_) => R::Error(w::Error {
                code: w::ErrorCode::Invalid as i32,
            }),
        },
        action @ (A::BeginUpload(_)
        | A::WriteFile(_)
        | A::InspectUpload(_)
        | A::CommitUpload(_)
        | A::AbortUpload(_)
        | A::CaptureFile(_)
        | A::ReadFile(_)
        | A::ReleaseFile(_)) => match files {
            Some(files) => match files.call(action).await {
                Ok(value) => value,
                Err(_) => R::Error(w::Error {
                    code: w::ErrorCode::Uncertain as i32,
                }),
            },
            None => R::Error(w::Error {
                code: w::ErrorCode::Rejected as i32,
            }),
        },
        A::Output(value) => match runner.output(&value).await {
            Ok(output) => R::Output(output),
            Err(_) => R::Error(w::Error {
                code: w::ErrorCode::Rejected as i32,
            }),
        },
    };
    Ok(w::Response {
        version: wire::VERSION,
        request_id: request.request_id,
        context: Some(actual.into()),
        result: Some(result),
    })
}

/// No TCP/Unix listener option. Only host CID 2 is accepted on this production entry point.
pub async fn serve_vsock(runner: Runner, tls: ServerTls, port: u32) -> Result<()> {
    serve_vsock_with_files(runner, None, tls, port).await
}
pub async fn serve_vsock_with_files(
    runner: Runner,
    files: Option<FileService>,
    tls: ServerTls,
    port: u32,
) -> Result<()> {
    use tokio_vsock::{VMADDR_CID_ANY, VMADDR_CID_HOST, VsockAddr, VsockListener};
    ensure!(port > 0 && port < u32::MAX, "invalid vsock port");
    let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port))?;
    let capacity = Arc::new(Semaphore::new(wire::MAX_CONNECTIONS));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _=tasks.join_next(),if !tasks.is_empty()=>{},
            accepted=listener.accept()=>{
                let (stream,peer)=accepted?;
                if peer.cid()!=VMADDR_CID_HOST {continue;}
                let Ok(permit)=capacity.clone().try_acquire_owned() else {continue};
                let runner=runner.clone();let tls=tls.clone();let files=files.clone();
                tasks.spawn(async move {let _permit=permit;let _=serve_connection_with_files(&runner,files.as_ref(),&tls,stream).await;});
            }
        }
    }
}
