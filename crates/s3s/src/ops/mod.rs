mod generated;
pub use self::generated::*;

mod signature;
use self::signature::SignatureContext;

mod get_object;

#[cfg(test)]
mod tests;

use crate::access::{S3Access, S3AccessContext};
use crate::auth::{Credentials, S3Auth};
use crate::error::*;
use crate::header;
use crate::host::S3Host;
use crate::http;
use crate::http::Body;
use crate::http::{OrderedHeaders, OrderedQs};
use crate::http::{Request, Response};
use crate::path::{ParseS3PathError, S3Path};
use crate::protocol::S3Request;
use crate::route::S3Route;
use crate::s3_trait::S3;
use crate::stream::VecByteStream;
use crate::stream::aggregate_unlimited;

use std::mem;
use std::net::{IpAddr, SocketAddr};
use std::ops::Not;
use std::sync::Arc;

use bytes::Bytes;
use hyper::HeaderMap;
use hyper::Method;
use hyper::StatusCode;
use hyper::Uri;
use hyper::http::HeaderValue;
use mime::Mime;
use tracing::debug;

#[async_trait::async_trait]
pub trait Operation: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    async fn call(&self, ccx: &CallContext<'_>, req: &mut Request) -> S3Result<Response>;
}

pub struct CallContext<'a> {
    pub s3: &'a Arc<dyn S3>,
    pub host: Option<&'a dyn S3Host>,
    pub auth: Option<&'a dyn S3Auth>,
    pub access: Option<&'a dyn S3Access>,
    pub route: Option<&'a dyn S3Route>,
}

fn build_s3_request<T>(input: T, req: &mut Request) -> S3Request<T> {
    let method = req.method.clone();
    let uri = mem::take(&mut req.uri);
    let headers = mem::take(&mut req.headers);
    let extensions = mem::take(&mut req.extensions);
    let credentials = req.s3ext.credentials.take();
    let region = req.s3ext.region.take();
    let service = req.s3ext.service.take();

    S3Request {
        input,
        method,
        uri,
        headers,
        extensions,
        credentials,
        region,
        service,
    }
}

pub(crate) fn serialize_error(mut e: S3Error, no_decl: bool) -> S3Result<Response> {
    let status = e.status_code().unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut res = Response::with_status(status);
    if no_decl {
        http::set_xml_body_no_decl(&mut res, &e)?;
    } else {
        http::set_xml_body(&mut res, &e)?;
    }
    if let Some(headers) = e.take_headers() {
        res.headers = headers;
    }
    drop(e);
    Ok(res)
}

fn unknown_operation() -> S3Error {
    S3Error::with_message(S3ErrorCode::NotImplemented, "Unknown operation")
}

fn extract_host(req: &Request) -> S3Result<Option<String>> {
    let Some(val) = req.headers.get(crate::header::HOST) else { return Ok(None) };
    let on_err = |e| s3_error!(e, InvalidRequest, "invalid header: Host: {val:?}");
    let host = val.to_str().map_err(on_err)?;
    Ok(Some(host.into()))
}

fn is_socket_addr_or_ip_addr(host: &str) -> bool {
    host.parse::<SocketAddr>().is_ok() || host.parse::<IpAddr>().is_ok()
}

fn convert_parse_s3_path_error(err: &ParseS3PathError) -> S3Error {
    match err {
        ParseS3PathError::InvalidPath => s3_error!(InvalidURI),
        ParseS3PathError::InvalidBucketName => s3_error!(InvalidBucketName),
        ParseS3PathError::KeyTooLong => s3_error!(KeyTooLongError),
    }
}

fn extract_qs(req_uri: &Uri) -> S3Result<Option<OrderedQs>> {
    let Some(query) = req_uri.query() else { return Ok(None) };
    match OrderedQs::parse(query) {
        Ok(ans) => Ok(Some(ans)),
        Err(source) => Err(S3Error::with_source(S3ErrorCode::InvalidURI, Box::new(source))),
    }
}

fn check_query_pattern(qs: &OrderedQs, name: &str, val: &str) -> bool {
    match qs.get_unique(name) {
        Some(v) => v == val,
        None => false,
    }
}

fn extract_headers(headers: &HeaderMap<HeaderValue>) -> S3Result<OrderedHeaders<'_>> {
    OrderedHeaders::from_headers(headers).map_err(|source| invalid_request!(source, "invalid headers"))
}

fn extract_mime(hs: &OrderedHeaders<'_>) -> S3Result<Option<Mime>> {
    let Some(content_type) = hs.get_unique(crate::header::CONTENT_TYPE) else { return Ok(None) };
    match content_type.parse::<Mime>() {
        Ok(x) => Ok(Some(x)),
        Err(e) => Err(invalid_request!(e, "invalid content type")),
    }
}

fn extract_content_length(req: &Request) -> Option<u64> {
    req.headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|val| atoi::atoi::<u64>(val.as_bytes()))
}

fn extract_decoded_content_length(hs: &'_ OrderedHeaders<'_>) -> S3Result<Option<usize>> {
    let Some(val) = hs.get_unique(crate::header::X_AMZ_DECODED_CONTENT_LENGTH) else { return Ok(None) };
    match atoi::atoi::<usize>(val.as_bytes()) {
        Some(x) => Ok(Some(x)),
        None => Err(invalid_request!("invalid header: x-amz-decoded-content-length")),
    }
}

async fn extract_full_body(content_length: Option<u64>, body: &mut Body) -> S3Result<Bytes> {
    if let Some(bytes) = body.bytes() {
        return Ok(bytes);
    }

    let bytes = body
        .store_all_unlimited()
        .await
        .map_err(|e| S3Error::with_source(S3ErrorCode::InternalError, e))?;

    if bytes.is_empty().not() {
        let content_length = content_length.ok_or(S3ErrorCode::MissingContentLength)?;
        if bytes.len() as u64 != content_length {
            return Err(s3_error!(IncompleteBody));
        }
    }

    Ok(bytes)
}

#[allow(clippy::declare_interior_mutable_const)]
fn fmt_content_length(len: usize) -> http::HeaderValue {
    const ZERO: http::HeaderValue = http::HeaderValue::from_static("0");
    if len > 0 {
        crate::utils::format::fmt_usize(len, |s| http::HeaderValue::try_from(s).unwrap())
    } else {
        ZERO
    }
}

pub async fn call(req: &mut Request, ccx: &CallContext<'_>) -> S3Result<Response> {
    let prep = match prepare(req, ccx).await {
        Ok(op) => op,
        Err(err) => {
            debug!(?err, "failed to prepare");
            return serialize_error(err, false);
        }
    };

    match prep {
        Prepare::S3(op) => {
            match op.call(ccx, req).await {
                Ok(resp) => {
                    Ok(resp) //
                }
                Err(err) => {
                    debug!(op = %op.name(), ?err, "op returns error");
                    serialize_error(err, false)
                }
            }
        }
        Prepare::CustomRoute => {
            let body = mem::take(&mut req.body);
            let mut s3_req = build_s3_request(body, req);
            let route = ccx.route.unwrap();

            let result = async {
                route.check_access(&mut s3_req).await?;
                route.call(s3_req).await
            }
            .await;

            match result {
                Ok(s3_resp) => Ok(Response {
                    status: s3_resp.status.unwrap_or_default(),
                    headers: s3_resp.headers,
                    body: s3_resp.output,
                    extensions: s3_resp.extensions,
                }),
                Err(err) => {
                    debug!(?err, "custom route returns error");
                    serialize_error(err, false)
                }
            }
        }
    }
}

enum Prepare {
    S3(&'static dyn Operation),
    CustomRoute,
}

#[allow(clippy::too_many_lines)]
async fn prepare(req: &mut Request, ccx: &CallContext<'_>) -> S3Result<Prepare> {
    let s3_path;
    let mut content_length;
    {
        let decoded_uri_path = urlencoding::decode(req.uri.path())
            .map_err(|_| S3ErrorCode::InvalidURI)?
            .into_owned();

        let host_header = extract_host(req)?;
        let vh;
        let vh_bucket;
        {
            let result = 'parse: {
                if let (Some(host_header), Some(s3_host)) = (host_header.as_deref(), ccx.host) {
                    if !is_socket_addr_or_ip_addr(host_header) {
                        debug!(?host_header, ?decoded_uri_path, "parsing virtual-hosted-style request");

                        vh = s3_host.parse_host_header(host_header)?;
                        debug!(?vh);

                        vh_bucket = vh.bucket();
                        break 'parse crate::path::parse_virtual_hosted_style(vh_bucket, &decoded_uri_path);
                    }
                }

                debug!(?decoded_uri_path, "parsing path-style request");
                vh_bucket = None;
                crate::path::parse_path_style(&decoded_uri_path)
            };

            req.s3ext.s3_path = Some(result.map_err(|err| convert_parse_s3_path_error(&err))?);
            s3_path = req.s3ext.s3_path.as_ref().unwrap();
        }

        req.s3ext.qs = extract_qs(&req.uri)?;
        content_length = extract_content_length(req);

        let hs = extract_headers(&req.headers)?;
        let mime = extract_mime(&hs)?;
        let decoded_content_length = extract_decoded_content_length(&hs)?;

        let body_changed;
        let transformed_body;
        {
            let mut scx = SignatureContext {
                auth: ccx.auth,

                req_method: &req.method,
                req_uri: &req.uri,
                req_body: &mut req.body,

                qs: req.s3ext.qs.as_ref(),
                hs,

                decoded_uri_path,
                vh_bucket,

                content_length,
                decoded_content_length,
                mime,

                multipart: None,
                transformed_body: None,
            };

            let credentials = scx.check().await?;

            body_changed = scx.transformed_body.is_some() || scx.multipart.is_some();
            transformed_body = scx.transformed_body;

            req.s3ext.multipart = scx.multipart;

            match credentials {
                Some(cred) => {
                    req.s3ext.credentials = Some(Credentials {
                        access_key: cred.access_key,
                        secret_key: cred.secret_key,
                    });
                    req.s3ext.region = cred.region;
                    req.s3ext.service = cred.service;
                }
                None => {
                    req.s3ext.credentials = None;
                }
            }
        }

        if body_changed {
            // invalidate the original content length
            if let Some(val) = req.headers.get_mut(header::CONTENT_LENGTH) {
                *val = fmt_content_length(decoded_content_length.unwrap_or(0));
            }
            if let Some(val) = &mut content_length {
                *val = 0;
            }
        }
        if let Some(body) = transformed_body {
            req.body = body;
        }

        let has_multipart = req.s3ext.multipart.is_some();
        debug!(?body_changed, ?decoded_content_length, ?has_multipart);
    }

    if let Some(route) = ccx.route {
        if route.is_match(&req.method, &req.uri, &req.headers, &mut req.extensions) {
            return Ok(Prepare::CustomRoute);
        }
    }

    let (op, needs_full_body) = 'resolve: {
        if let Some(multipart) = &mut req.s3ext.multipart {
            if req.method == Method::POST {
                match s3_path {
                    S3Path::Root => return Err(unknown_operation()),
                    S3Path::Bucket { .. } => {
                        // POST object
                        debug!(?multipart);
                        let file_stream = multipart.take_file_stream().expect("missing file stream");
                        let vec_bytes = aggregate_unlimited(file_stream).await.map_err(S3Error::internal_error)?;
                        let vec_stream = VecByteStream::new(vec_bytes);
                        req.s3ext.vec_stream = Some(vec_stream);
                        break 'resolve (&PutObject as &'static dyn Operation, false);
                    }
                    // FIXME: POST /bucket/key hits this branch
                    S3Path::Object { .. } => return Err(s3_error!(MethodNotAllowed)),
                }
            }
        }
        resolve_route(req, s3_path, req.s3ext.qs.as_ref())?
    };

    // FIXME: hack for E2E tests (minio/mint)
    if op.name() == "ListObjects" {
        if let Some(qs) = req.s3ext.qs.as_ref() {
            if qs.has("events") {
                return Err(s3_error!(NotImplemented, "listenBucketNotification only works on MinIO"));
            }
        }
    }

    debug!(op = %op.name(), ?s3_path, "resolved route");

    if ccx.auth.is_some() {
        let mut acx = S3AccessContext {
            credentials: req.s3ext.credentials.as_ref(),
            s3_path,
            s3_op: &crate::S3Operation { name: op.name() },
            method: &req.method,
            uri: &req.uri,
            headers: &req.headers,
            extensions: &mut req.extensions,
        };
        match ccx.access {
            Some(access) => access.check(&mut acx).await?,
            None => crate::access::default_check(&mut acx)?,
        }
    }

    debug!(op = %op.name(), ?s3_path, "checked access");

    if needs_full_body {
        extract_full_body(content_length, &mut req.body).await?;
    }

    Ok(Prepare::S3(op))
}
