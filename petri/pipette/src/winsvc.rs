// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to run pipette as a Windows service.

use crate::agent::Agent;
use anyhow::Context;
use futures_concurrency::future::Race;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use std::ffi::OsString;
use std::time::Duration;
use windows_service::define_windows_service;
use windows_service::service;
use windows_service::service_control_handler;
use windows_service::service_control_handler::ServiceControlHandlerResult;
use windows_service::service_dispatcher;

const SERVICE_NAME: &str = "pipette";

pub fn start_service() -> anyhow::Result<()> {
    // TODO: retarget stderr somewhere that the host can see (serial port?)
    define_windows_service!(ffi_service_main, service_main);
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).context("failed to start service")?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    DefaultPool::run_with(async |driver| {
        if let Err(e) = service_main_inner(driver).await {
            eprintln!("service_main failed: {:#}", e);
        }
    })
}

async fn service_main_inner(driver: DefaultDriver) -> anyhow::Result<()> {
    let (send, recv) = mesh::oneshot::<()>();
    let mut send = Some(send);
    let event_handler = move |control_event| match control_event {
        service::ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        service::ServiceControl::Stop => {
            let _ = send.take();
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
    let set_status = |current_state| {
        status_handle
            .set_service_status(service::ServiceStatus {
                service_type: service::ServiceType::OWN_PROCESS,
                current_state,
                controls_accepted: service::ServiceControlAccept::STOP,
                exit_code: service::ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::ZERO,
                process_id: None,
            })
            .context("failed to set service status")
    };

    set_status(service::ServiceState::StartPending)?;

    let run = async {
        let agent = Agent::new(driver).await?;
        set_status(service::ServiceState::Running)?;
        agent.run().await
    };

    let stop = async {
        recv.await.ok();
        set_status(service::ServiceState::StopPending)
    };

    let r = (run, stop).race().await;

    let exit_code = match r {
        Ok(()) => 0,
        Err(_) => 1,
    };

    status_handle
        .set_service_status(service::ServiceStatus {
            service_type: service::ServiceType::OWN_PROCESS,
            current_state: service::ServiceState::Stopped,
            controls_accepted: service::ServiceControlAccept::STOP,
            exit_code: service::ServiceExitCode::Win32(exit_code),
            checkpoint: 0,
            wait_hint: Duration::ZERO,
            process_id: None,
        })
        .ok();

    r
}
