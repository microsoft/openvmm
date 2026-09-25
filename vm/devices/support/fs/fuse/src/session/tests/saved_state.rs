// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for saving, validating, and restoring the FUSE session state.

use super::*;
use crate::session::saved_state::SessionInfoState;
use crate::session::saved_state::SessionState;
use crate::session::saved_state::SessionStateError;

#[test]
fn initialized_session_state_restores_without_a_second_init() {
    let mut init_sender = MockSender::default();
    let source = Session::new(TestFs::default());
    source.dispatch(
        Request::new(FUSE_INIT_REQUEST).unwrap(),
        &mut init_sender,
        None,
    );
    let saved = source.save_state();
    assert!(saved.initialized);

    let fs = TestFs::default();
    let state = Arc::clone(&fs.state);
    let destination = Session::new(fs);
    destination.restore_state(saved).unwrap();
    assert!(destination.is_initialized());

    let mut sender = MockSender { state: 1 };
    destination.dispatch(
        Request::new(FUSE_GETATTR_REQUEST).unwrap(),
        &mut sender,
        None,
    );
    assert_eq!(state.lock().called, GETATTR_CALLED);
}

#[test]
fn malformed_session_state_is_rejected() {
    let state = SessionState {
        initialized: true,
        info: SessionInfoState {
            major: FUSE_KERNEL_VERSION,
            minor: 27,
            capable: FUSE_ASYNC_READ,
            want: FUSE_ASYNC_READ | FUSE_BIG_WRITES,
            max_write: PAGE_SIZE,
            time_gran: 1,
            ..Default::default()
        },
    };
    assert!(matches!(
        state.validate(),
        Err(SessionStateError::InvalidFlags)
    ));

    let state = SessionState {
        initialized: false,
        info: SessionInfoState {
            max_write: PAGE_SIZE,
            ..Default::default()
        },
    };
    assert!(matches!(
        state.validate(),
        Err(SessionStateError::UninitializedWithInfo)
    ));

    let state = SessionState {
        initialized: true,
        info: SessionInfoState {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION + 1,
            max_write: PAGE_SIZE,
            time_gran: 1,
            ..Default::default()
        },
    };
    assert!(matches!(
        state.validate(),
        Err(SessionStateError::UnsupportedVersion)
    ));
}

#[test]
fn init_newer_minor_is_capped_to_the_supported_version() {
    let request_data = make_init_request(
        FUSE_KERNEL_VERSION,
        FUSE_KERNEL_MINOR_VERSION + 1,
        131072,
        0,
        0,
    );
    let session = Session::new(InitCapturingFs::default());
    let mut sender = CapturingSender::default();

    session.dispatch(
        Request::new(request_data.as_slice()).unwrap(),
        &mut sender,
        None,
    );

    assert_eq!(session.save_state().info.minor, FUSE_KERNEL_MINOR_VERSION);
}
