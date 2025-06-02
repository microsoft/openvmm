// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HTTP blob implementation based on [`hyper`], [`tokio`], and
//! [`hyper_tls`].
//!
//! In the future, it may better to use `pal_async` instead. This will require a
//! new, unreleased version of `hyper`, and a bunch of infrastructure to support
//! initiating TCP connections the way `hyper` expects.

use super::Blob;
use anyhow::Context as _;
use async_trait::async_trait;
use http::uri::Scheme;
use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::Request;
use hyper::StatusCode;
use hyper::Uri;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use inspect::Inspect;
use once_cell::sync::OnceCell;
use std::fmt::Debug;
use std::io;

/// A blob backed by an HTTP/HTTPS connection.
#[derive(Debug, Inspect)]
pub struct HttpBlob {
    #[inspect(skip)]
    client: Client<HttpsConnector<HttpConnector>, Empty<&'static [u8]>>,
    #[inspect(debug)]
    version: http::Version,
    #[inspect(display)]
    uri: Uri,
    len: u64,
    #[inspect(skip)]
    tokio_handle: tokio::runtime::Handle,
}

static TOKIO_RUNTIME: OnceCell<tokio::runtime::Runtime> = OnceCell::new();

impl HttpBlob {
    /// Connects to `url` and returns an object to access it as a blob.
    pub async fn new(url: &str) -> anyhow::Result<Self> {
        let mut uri: Uri = url.parse()?;

        let connector = HttpsConnector::new();
        let builder = Client::builder(TokioExecutor::new());
        let client = builder.build(connector);

        let handle = TOKIO_RUNTIME
            .get_or_try_init(tokio::runtime::Runtime::new)
            .context("failed to initialize tokio")?
            .handle()
            .clone();

        let mut redirect_count = 0;
        let response = loop {
            if redirect_count > 5 {
                anyhow::bail!("too many redirects");
            }

            let response = handle
                .spawn(
                    client.request(
                        Request::builder()
                            .uri(&uri)
                            .method("HEAD")
                            .body(Empty::new())
                            .unwrap(),
                    ),
                )
                .await
                .unwrap()
                .context("failed to query blob size")?;

            let next_uri: Uri = match response.status() {
                StatusCode::OK => break response,
                StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT => response
                    .headers()
                    .get("Location")
                    .context("missing redirect URL")?
                    .to_str()
                    .context("couldn't parse redirect URL")?
                    .parse()
                    .context("couldn't parse redirect URL")?,
                status => {
                    anyhow::bail!("failed to query blob size: {status}");
                }
            };

            if uri.scheme() == Some(&Scheme::HTTPS) && next_uri.scheme() != Some(&Scheme::HTTPS) {
                anyhow::bail!("https redirected to http");
            }

            uri = next_uri;
            redirect_count += 1;
        };

        let len = response
            .headers()
            .get("Content-Length")
            .context("missing blob length")?
            .to_str()
            .context("couldn't parse blob length")?
            .parse()
            .context("couldn't parse blob length")?;

        let version = response.version();

        Ok(Self {
            client,
            version,
            uri,
            len,
            tokio_handle: handle,
        })
    }
}

#[async_trait]
impl Blob for HttpBlob {
    async fn read(&self, mut buf: &mut [u8], offset: u64) -> io::Result<()> {
        let mut response = self
            .tokio_handle
            .spawn(
                self.client.request(
                    Request::builder()
                        .uri(&self.uri)
                        .header(
                            hyper::header::RANGE,
                            format!("bytes={}-{}", offset, offset + buf.len() as u64 - 1,),
                        )
                        .body(Empty::new())
                        .unwrap(),
                ),
            )
            .await
            .unwrap()
            .map_err(io::Error::other)?;

        if !response.status().is_success() {
            return Err(io::Error::other(response.status().to_string()));
        }

        while let Some(frame) = response.body_mut().frame().await {
            let frame = frame.map_err(io::Error::other)?;
            if let Some(data) = frame.data_ref() {
                let len = data.len();
                if len > buf.len() {
                    return Err(io::Error::other("server did not respect range query"));
                }
                let (this, rest) = buf.split_at_mut(len);
                this.copy_from_slice(data);
                buf = rest;
            }
        }

        if !buf.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }

        Ok(())
    }

    fn len(&self) -> u64 {
        self.len
    }
}
