// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use fuse::Mapper;
use fuse::ReplySender;
use fuse::Request;

pub(crate) struct VirtioFsSession {
    session: fuse::Session,
    #[cfg(windows)]
    user_token: Option<std::os::windows::io::OwnedHandle>,
}

impl VirtioFsSession {
    pub(crate) fn new<Fs: fuse::Fuse + Send + Sync + 'static>(fs: Fs) -> Self {
        Self {
            session: fuse::Session::new(fs),
            #[cfg(windows)]
            user_token: None,
        }
    }

    #[cfg(windows)]
    pub(crate) fn with_user_token(
        mut self,
        token: Option<std::os::windows::io::OwnedHandle>,
    ) -> Self {
        self.user_token = token;
        self
    }

    pub(crate) fn dispatch(
        &self,
        request: Request,
        sender: &mut impl ReplySender,
        mapper: Option<&dyn Mapper>,
    ) {
        #[cfg(windows)]
        let _impersonation = match self
            .user_token
            .as_ref()
            .map(crate::impersonation::ImpersonationGuard::new)
            .transpose()
        {
            Ok(guard) => guard,
            Err(e) => {
                tracelimit::error_ratelimited!(
                    error = &e as &dyn std::error::Error,
                    "failed to impersonate user token"
                );
                if let Err(e) = sender.send_error(request.unique(), lx::Error::EIO.value()) {
                    tracelimit::error_ratelimited!(
                        error = &e as &dyn std::error::Error,
                        "failed to send error reply"
                    );
                }
                return;
            }
        };

        self.session.dispatch(request, sender, mapper);
    }

    pub(crate) fn destroy(&self) {
        self.session.destroy();
    }
}
