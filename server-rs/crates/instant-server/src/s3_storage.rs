use std::fmt::Write;

use axum::body::Body;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::{DateTime, Utc};
use futures_util::{StreamExt, TryStreamExt};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::header::{
    AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, HOST, HeaderMap,
    HeaderName, HeaderValue, LAST_MODIFIED,
};
use reqwest::{Method, Url};
use sha2::{Digest, Sha256};

use crate::{
    BackupObjectEntry, BackupObjectStore, ServerConfig, StorageObjectMetadata, StorageObjectReader,
    StorageObjectStore, StoragePutRequest,
};

type HmacSha256 = Hmac<Sha256>;

/// Credentials and endpoint selection for the S3-compatible storage bucket.
pub struct S3ObjectStoreConfig {
    pub endpoint: String,
    pub download_endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
    pub force_path_style: bool,
    pub download_expiry_seconds: u32,
}

/// Endpoint and bucket settings for an S3-compatible store.
pub struct S3ObjectStoreRuntimeConfig {
    pub endpoint: String,
    pub download_endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub force_path_style: bool,
    pub download_expiry_seconds: u32,
}

/// Optional long-lived credentials used only to sign download URLs.
///
/// Deployments set these separately from the request credentials when
/// presigned URLs must remain valid for seven days. This type deliberately
/// does not implement `Debug`.
#[derive(Clone)]
pub struct S3PresigningCredentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

#[derive(Clone)]
enum S3CredentialSource {
    Static {
        access_key: String,
        secret_key: String,
        session_token: Option<String>,
    },
}

struct ResolvedS3Credentials {
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
}

/// Streaming S3-compatible implementation of the storage object boundary.
pub struct S3ObjectStore {
    client: reqwest::Client,
    endpoint: Url,
    download_endpoint: Url,
    region: String,
    bucket: String,
    request_credentials: S3CredentialSource,
    download_credentials: S3CredentialSource,
    force_path_style: bool,
    download_expiry_seconds: u32,
}

impl S3ObjectStore {
    /// Builds an object store after validating endpoint, bucket, region, and expiry.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for non-HTTP(S) endpoints, invalid bucket/region
    /// characters, empty credentials, or an expiry outside S3's seven-day limit.
    pub fn new(config: S3ObjectStoreConfig) -> Result<Self, String> {
        if config.access_key.is_empty() || config.secret_key.is_empty() {
            return Err("S3 credentials must not be empty".to_owned());
        }
        let credentials = S3CredentialSource::Static {
            access_key: config.access_key,
            secret_key: config.secret_key,
            session_token: config.session_token,
        };
        Self::build(
            S3ObjectStoreRuntimeConfig {
                endpoint: config.endpoint,
                download_endpoint: config.download_endpoint,
                region: config.region,
                bucket: config.bucket,
                force_path_style: config.force_path_style,
                download_expiry_seconds: config.download_expiry_seconds,
            },
            credentials.clone(),
            credentials,
        )
    }

    fn build(
        config: S3ObjectStoreRuntimeConfig,
        request_credentials: S3CredentialSource,
        download_credentials: S3CredentialSource,
    ) -> Result<Self, String> {
        let endpoint = Url::parse(&config.endpoint).map_err(|error| error.to_string())?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err("S3 endpoint must be an absolute HTTP(S) URL".to_owned());
        }
        let download_endpoint = config.download_endpoint.as_ref().map_or_else(
            || Ok(endpoint.clone()),
            |value| {
                let endpoint = Url::parse(value).map_err(|error| error.to_string())?;
                if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
                    return Err("S3 download endpoint must be an absolute HTTP(S) URL".to_owned());
                }
                Ok(endpoint)
            },
        )?;
        if !valid_name(&config.bucket) {
            return Err("S3 bucket contains invalid characters".to_owned());
        }
        if !valid_name(&config.region) {
            return Err("S3 region contains invalid characters".to_owned());
        }
        if !(1..=604_800).contains(&config.download_expiry_seconds) {
            return Err("S3 download expiry must be between 1 and 604800 seconds".to_owned());
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| error.to_string())?,
            endpoint,
            download_endpoint,
            region: config.region,
            bucket: config.bucket,
            request_credentials,
            download_credentials,
            force_path_style: config.force_path_style,
            download_expiry_seconds: config.download_expiry_seconds,
        })
    }

    #[allow(clippy::unused_async)]
    async fn credentials(source: &S3CredentialSource) -> Result<ResolvedS3Credentials, String> {
        match source {
            S3CredentialSource::Static {
                access_key,
                secret_key,
                session_token,
            } => Ok(ResolvedS3Credentials {
                access_key: access_key.clone(),
                secret_key: secret_key.clone(),
                session_token: session_token.clone(),
            }),
        }
    }

    fn object_url(&self, key: &str) -> Result<Url, String> {
        self.object_url_at_endpoint(&self.endpoint, &self.bucket, key)
    }

    fn bucket_url(&self) -> Result<Url, String> {
        self.object_url_at_endpoint(&self.endpoint, &self.bucket, "")
    }

    fn object_url_in_bucket(&self, bucket: &str, key: &str) -> Result<Url, String> {
        self.object_url_at_endpoint(&self.endpoint, bucket, key)
    }

    fn download_object_url(&self, key: &str) -> Result<Url, String> {
        self.object_url_at_endpoint(&self.download_endpoint, &self.bucket, key)
    }

    fn object_url_at_endpoint(
        &self,
        endpoint: &Url,
        bucket: &str,
        key: &str,
    ) -> Result<Url, String> {
        if !valid_name(bucket) {
            return Err("S3 bucket contains invalid characters".to_owned());
        }
        if !key.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'$')
        }) {
            return Err("S3 object key contains unsupported characters".to_owned());
        }
        if key.starts_with('/') || key.split('/').any(|segment| matches!(segment, "." | "..")) {
            return Err("S3 object key contains an unsafe path segment".to_owned());
        }
        let mut url = endpoint.clone();
        let base_path = url.path().trim_end_matches('/').to_owned();
        if self.force_path_style {
            url.set_path(&format!("{base_path}/{bucket}/{key}"));
        } else {
            let host = url
                .host_str()
                .ok_or_else(|| "S3 endpoint has no host".to_owned())?;
            url.set_host(Some(&format!("{bucket}.{host}")))
                .map_err(|error| error.to_string())?;
            url.set_path(&format!("{base_path}/{key}"));
        }
        Ok(url)
    }

    async fn signed_headers(
        &self,
        method: &Method,
        url: &Url,
        payload_hash: &str,
        now: DateTime<Utc>,
        extra: &[(String, String)],
    ) -> Result<HeaderMap, String> {
        let credentials = Self::credentials(&self.request_credentials).await?;
        let date = now.format("%Y%m%d").to_string();
        let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
        let host = host_header(url)?;
        let mut canonical_headers = vec![
            ("host".to_owned(), host.clone()),
            ("x-amz-content-sha256".to_owned(), payload_hash.to_owned()),
            ("x-amz-date".to_owned(), timestamp.clone()),
        ];
        if let Some(token) = &credentials.session_token {
            canonical_headers.push(("x-amz-security-token".to_owned(), token.clone()));
        }
        canonical_headers.extend(extra.iter().cloned());
        canonical_headers.sort_by(|left, right| left.0.cmp(&right.0));
        let signed_header_names = canonical_headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let mut canonical_header_text = String::new();
        for (name, value) in &canonical_headers {
            writeln!(canonical_header_text, "{name}:{}", value.trim())
                .expect("writing to a string is infallible");
        }
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method.as_str(),
            canonical_s3_path(url.path()),
            url.query().unwrap_or_default(),
            canonical_header_text,
            signed_header_names,
            payload_hash,
        );
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let signature = self.signature(&credentials.secret_key, &date, &string_to_sign)?;
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_header_names}, Signature={signature}",
            credentials.access_key
        );
        let mut headers = HeaderMap::new();
        insert_header(&mut headers, HOST, &host)?;
        insert_header(
            &mut headers,
            HeaderName::from_static("x-amz-content-sha256"),
            payload_hash,
        )?;
        insert_header(
            &mut headers,
            HeaderName::from_static("x-amz-date"),
            &timestamp,
        )?;
        if let Some(token) = &credentials.session_token {
            insert_header(
                &mut headers,
                HeaderName::from_static("x-amz-security-token"),
                token,
            )?;
        }
        for (name, value) in extra {
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|error| error.to_string())?;
            insert_header(&mut headers, name, value)?;
        }
        insert_header(&mut headers, AUTHORIZATION, &authorization)?;
        Ok(headers)
    }

    fn signature(
        &self,
        secret_key: &str,
        date: &str,
        string_to_sign: &str,
    ) -> Result<String, String> {
        let date_key = hmac(format!("AWS4{secret_key}").as_bytes(), date.as_bytes())?;
        let region_key = hmac(&date_key, self.region.as_bytes())?;
        let service_key = hmac(&region_key, b"s3")?;
        let signing_key = hmac(&service_key, b"aws4_request")?;
        Ok(hex(&hmac(&signing_key, string_to_sign.as_bytes())?))
    }

    /// Presigns a GET. The stored object's `Content-Type` and
    /// `Content-Disposition` decide how the browser treats it, so signing never
    /// reads object metadata and stays pure computation.
    async fn presigned_url(&self, key: &str, now: DateTime<Utc>) -> Result<String, String> {
        let credentials = Self::credentials(&self.download_credentials).await?;
        let mut url = self.download_object_url(key)?;
        let date = now.format("%Y%m%d").to_string();
        let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let host = host_header(&url)?;
        let mut query = vec![
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256".to_owned()),
            (
                "X-Amz-Credential",
                format!("{}/{scope}", credentials.access_key),
            ),
            ("X-Amz-Date", timestamp.clone()),
            ("X-Amz-Expires", self.download_expiry_seconds.to_string()),
            ("X-Amz-SignedHeaders", "host".to_owned()),
        ];
        if let Some(token) = &credentials.session_token {
            query.push(("X-Amz-Security-Token", token.clone()));
        }
        let canonical_query = canonical_query(&query);
        let canonical_request = format!(
            "GET\n{}\n{canonical_query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD",
            canonical_s3_path(url.path())
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let signature = self.signature(&credentials.secret_key, &date, &string_to_sign)?;
        url.set_query(Some(&format!(
            "{canonical_query}&X-Amz-Signature={signature}"
        )));
        Ok(url.to_string())
    }

    async fn signed_download_url_for_app_at(
        &self,
        _app_id: uuid::Uuid,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<String, String> {
        self.object_url(key)?;
        self.presigned_url(key, now).await
    }

    async fn signed_download_urls_for_app_at(
        &self,
        _app_id: uuid::Uuid,
        keys: Vec<String>,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, String> {
        for key in &keys {
            self.object_url(key)?;
        }
        let mut urls = Vec::with_capacity(keys.len());
        for key in keys {
            urls.push(self.presigned_url(&key, now).await?);
        }
        Ok(urls)
    }

    async fn metadata(&self, key: &str) -> Result<StorageObjectMetadata, String> {
        let url = self.object_url(key)?;
        let headers = self
            .signed_headers(&Method::HEAD, &url, "UNSIGNED-PAYLOAD", Utc::now(), &[])
            .await?;
        let response = self
            .client
            .head(url)
            .headers(headers)
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?;
        let headers = response.headers();
        let size = headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| "S3 HEAD response omitted content-length".to_owned())?;
        Ok(StorageObjectMetadata {
            size,
            content_type: header_string(headers, CONTENT_TYPE),
            content_disposition: header_string(headers, CONTENT_DISPOSITION),
            etag: header_string(headers, ETAG),
            last_modified_ms: header_string(headers, LAST_MODIFIED)
                .and_then(|value| DateTime::parse_from_rfc2822(&value).ok())
                .map(|value| value.timestamp_millis()),
        })
    }

    fn upload_tag_headers(
        &self,
        tags: &std::collections::BTreeMap<String, String>,
    ) -> Vec<(String, String)> {
        if tags.is_empty() {
            return Vec::new();
        }
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        serializer.extend_pairs(tags.iter());
        let encoded = serializer.finish();
        let header = if is_cloudflare_r2_endpoint(&self.endpoint) {
            // R2 rejects S3 object tagging. Preserve Instant's backup policy
            // marker as supported custom metadata instead of failing PutObject.
            "x-amz-meta-instant-tags"
        } else {
            "x-amz-tagging"
        };
        vec![(header.to_owned(), encoded)]
    }
}

/// Builds the production S3-compatible adapter from process configuration.
///
/// `S3_ENDPOINT` is required and is used for API requests, while
/// `S3_PUBLIC_ENDPOINT` is used for browser-facing download signatures. This
/// deliberately does not infer a provider endpoint: self-hosted deployments
/// must choose their object store explicitly (for example Cloudflare R2,
/// `MinIO`, or another S3-compatible service). When supplied, `presigning` is
/// used only for download URLs.
///
/// # Errors
///
/// Returns an error for invalid storage configuration, a missing endpoint, or
/// missing object-store credentials.
pub async fn s3_object_store_from_config(
    config: &ServerConfig,
    presigning: Option<S3PresigningCredentials>,
) -> Result<S3ObjectStore, String> {
    s3_object_store_for_bucket_from_config(config, &config.storage().bucket, presigning).await
}

/// Builds an S3-compatible adapter using the shared endpoint and credential
/// configuration but targeting a distinct bucket. Backups use this boundary
/// so they never fall through to the user-file storage bucket.
///
/// # Errors
///
/// Returns an error for invalid storage configuration, a missing endpoint, or
/// missing object-store credentials.
pub async fn s3_object_store_for_bucket_from_config(
    config: &ServerConfig,
    bucket: &str,
    presigning: Option<S3PresigningCredentials>,
) -> Result<S3ObjectStore, String> {
    let runtime = configured_runtime(config, bucket)?;
    let request_credentials = static_s3_credentials_from_lookup(|name| std::env::var(name).ok())?;
    let download_credentials = presigning.map_or_else(
        || request_credentials.clone(),
        |credentials| S3CredentialSource::Static {
            access_key: credentials.access_key,
            secret_key: credentials.secret_key,
            session_token: credentials.session_token,
        },
    );
    S3ObjectStore::build(runtime, request_credentials, download_credentials)
}

fn configured_runtime(
    config: &ServerConfig,
    bucket: &str,
) -> Result<S3ObjectStoreRuntimeConfig, String> {
    let storage = config.storage();
    let endpoint = storage.endpoint.clone().ok_or_else(|| {
        "S3_ENDPOINT is required; configure an S3-compatible object store (for example R2 or MinIO)"
            .to_owned()
    })?;
    Ok(S3ObjectStoreRuntimeConfig {
        endpoint,
        download_endpoint: storage.public_endpoint.clone(),
        region: storage.region.clone(),
        bucket: bucket.to_owned(),
        force_path_style: !storage.virtual_host_style,
        download_expiry_seconds: storage.download_url_ttl_seconds,
    })
}

fn static_s3_credentials_from_lookup(
    mut get: impl FnMut(&str) -> Option<String>,
) -> Result<S3CredentialSource, String> {
    let mut non_blank = |name: &str| get(name).filter(|value| !value.trim().is_empty());
    let neutral_access_key = non_blank("INSTANT_S3_ACCESS_KEY_ID");
    let neutral_secret_key = non_blank("INSTANT_S3_SECRET_ACCESS_KEY");
    let (access_key, secret_key, session_token) = match (neutral_access_key, neutral_secret_key) {
        (Some(access_key), Some(secret_key)) => (
            access_key,
            secret_key,
            non_blank("INSTANT_S3_SESSION_TOKEN"),
        ),
        (Some(_), None) | (None, Some(_)) => {
            return Err(
                "INSTANT_S3_ACCESS_KEY_ID and INSTANT_S3_SECRET_ACCESS_KEY must be set together"
                    .to_owned(),
            );
        }
        (None, None) => {
            return Err(
                "INSTANT_S3_ACCESS_KEY_ID is required for S3-compatible storage".to_owned(),
            );
        }
    };
    Ok(S3CredentialSource::Static {
        access_key,
        secret_key,
        session_token,
    })
}

impl S3ObjectStore {
    pub async fn copy_to_bucket(
        &self,
        key: String,
        destination_bucket: String,
    ) -> Result<(), String> {
        let url = self.object_url_in_bucket(&destination_bucket, &key)?;
        let copy_source = format!("/{}/{}", self.bucket, key);
        let headers = self
            .signed_headers(
                &Method::PUT,
                &url,
                "UNSIGNED-PAYLOAD",
                Utc::now(),
                &[("x-amz-copy-source".to_owned(), copy_source)],
            )
            .await?;
        self.client
            .put(url)
            .headers(headers)
            .body(reqwest::Body::from(Vec::new()))
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

impl StorageObjectReader for S3ObjectStore {
    async fn read_object(
        &self,
        bucket: String,
        key: String,
        max_bytes: usize,
    ) -> Result<Vec<u8>, String> {
        let url = self.object_url_in_bucket(&bucket, &key)?;
        let headers = self
            .signed_headers(&Method::GET, &url, "UNSIGNED-PAYLOAD", Utc::now(), &[])
            .await?;
        let response = self
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?;
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            return Err(format!("S3 object exceeds the {max_bytes}-byte read limit"));
        }
        let mut output = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| error.to_string())?;
            if output.len().saturating_add(chunk.len()) > max_bytes {
                return Err(format!("S3 object exceeds the {max_bytes}-byte read limit"));
            }
            output.extend_from_slice(&chunk);
        }
        Ok(output)
    }
}

impl BackupObjectStore for S3ObjectStore {
    async fn list_backup_objects(&self, prefix: String) -> Result<Vec<BackupObjectEntry>, String> {
        let mut objects = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut url = self.bucket_url()?;
            let mut query = vec![("list-type", "2".to_owned()), ("prefix", prefix.clone())];
            if let Some(token) = &continuation {
                query.push(("continuation-token", token.clone()));
            }
            url.set_query(Some(&canonical_query(&query)));
            let headers = self
                .signed_headers(&Method::GET, &url, "UNSIGNED-PAYLOAD", Utc::now(), &[])
                .await?;
            let body = self
                .client
                .get(url)
                .headers(headers)
                .send()
                .await
                .map_err(|error| error.to_string())?
                .error_for_status()
                .map_err(|error| error.to_string())?
                .text()
                .await
                .map_err(|error| error.to_string())?;
            objects.extend(parse_list_objects(&body)?);
            if tag_text(&body, "IsTruncated").as_deref() != Some("true") {
                break;
            }
            continuation = Some(
                tag_text(&body, "NextContinuationToken")
                    .ok_or_else(|| "truncated S3 listing omitted continuation token".to_owned())?,
            );
        }
        Ok(objects)
    }

    async fn sign_backup_object(&self, key: String) -> Result<String, String> {
        self.signed_download_url(key).await
    }

    async fn sign_backup_object_for_app(
        &self,
        app_id: uuid::Uuid,
        key: String,
    ) -> Result<String, String> {
        self.signed_download_url_for_app_at(app_id, &key, Utc::now())
            .await
    }

    async fn read_backup_object(
        &self,
        key: String,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, String> {
        match self.read_object(self.bucket.clone(), key, max_bytes).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.contains("404") || error.contains("NoSuchKey") => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn stream_backup_object(&self, key: String) -> Result<Option<Body>, String> {
        let url = self.object_url(&key)?;
        let headers = self
            .signed_headers(&Method::GET, &url, "UNSIGNED-PAYLOAD", Utc::now(), &[])
            .await?;
        let response = self
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response
            .error_for_status()
            .map_err(|error| error.to_string())?;
        let stream = response
            .bytes_stream()
            .map_err(|error| std::io::Error::other(error.to_string()));
        Ok(Some(Body::from_stream(stream)))
    }
}

impl StorageObjectStore for S3ObjectStore {
    async fn put(
        &self,
        request: StoragePutRequest,
        body: Body,
    ) -> Result<StorageObjectMetadata, String> {
        let url = self.object_url(&request.key)?;
        let mut extra = self.upload_tag_headers(&request.tags);
        if let Some(content_encoding) = &request.content_encoding {
            extra.push(("content-encoding".to_owned(), content_encoding.clone()));
        }
        let signed = self
            .signed_headers(&Method::PUT, &url, "UNSIGNED-PAYLOAD", Utc::now(), &extra)
            .await?;
        let mut upload = self.client.put(url).headers(signed);
        if let Some(content_type) = request.content_type {
            upload = upload.header(CONTENT_TYPE, content_type);
        }
        if let Some(content_disposition) = request.content_disposition {
            upload = upload.header(CONTENT_DISPOSITION, content_disposition);
        }
        if let Some(content_length) = request.content_length {
            upload = upload.header(CONTENT_LENGTH, content_length);
        }
        let response = upload
            .body(reqwest::Body::wrap_stream(body.into_data_stream()))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("S3 PUT returned {status}: {body}"));
        }
        self.metadata(&request.key).await
    }

    async fn delete(&self, key: String) -> Result<(), String> {
        let url = self.object_url(&key)?;
        let headers = self
            .signed_headers(&Method::DELETE, &url, "UNSIGNED-PAYLOAD", Utc::now(), &[])
            .await?;
        self.client
            .delete(url)
            .headers(headers)
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn delete_many(&self, keys: Vec<String>) -> Result<(), String> {
        if keys.is_empty() {
            return Ok(());
        }
        if keys.len() > 1_000 {
            return Err("S3 DeleteObjects supports at most 1000 keys".to_owned());
        }
        for key in &keys {
            self.object_url(key)?;
        }
        let body = delete_objects_xml(&keys);
        let digest = Sha256::digest(body.as_bytes());
        let payload_hash = hex(&digest);
        let checksum = BASE64_STANDARD.encode(digest);
        let mut url = self.bucket_url()?;
        url.set_query(Some("delete="));
        let headers = self
            .signed_headers(
                &Method::POST,
                &url,
                &payload_hash,
                Utc::now(),
                &[
                    ("content-type".to_owned(), "application/xml".to_owned()),
                    ("x-amz-checksum-sha256".to_owned(), checksum),
                ],
            )
            .await?;
        let response = self
            .client
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?;
        let response = response.text().await.map_err(|error| error.to_string())?;
        if let Some(error) = delete_result_error(&response) {
            return Err(error);
        }
        Ok(())
    }

    async fn signed_download_url(&self, key: String) -> Result<String, String> {
        self.presigned_url(&key, Utc::now()).await
    }

    async fn signed_download_url_for_app(
        &self,
        app_id: uuid::Uuid,
        key: String,
    ) -> Result<String, String> {
        self.signed_download_url_for_app_at(app_id, &key, Utc::now())
            .await
    }

    async fn signed_download_urls_for_app(
        &self,
        app_id: uuid::Uuid,
        keys: Vec<String>,
    ) -> Result<Vec<String>, String> {
        self.signed_download_urls_for_app_at(app_id, keys, Utc::now())
            .await
    }
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn is_cloudflare_r2_endpoint(endpoint: &Url) -> bool {
    endpoint
        .host_str()
        .is_some_and(|host| host.ends_with(".r2.cloudflarestorage.com"))
}

fn host_header(url: &Url) -> Result<String, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "S3 endpoint has no host".to_owned())?;
    Ok(url
        .port()
        .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}")))
}

fn insert_header(headers: &mut HeaderMap, name: HeaderName, value: &str) -> Result<(), String> {
    headers.insert(
        name,
        HeaderValue::from_str(value).map_err(|error| error.to_string())?,
    );
    Ok(())
}

fn header_string(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn canonical_query(values: &[(&str, String)]) -> String {
    let mut values = values
        .iter()
        .map(|(key, value)| (aws_encode(key), aws_encode(value)))
        .collect::<Vec<_>>();
    values.sort();
    values
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn parse_list_objects(xml: &str) -> Result<Vec<BackupObjectEntry>, String> {
    let mut objects = Vec::new();
    let mut remaining = xml;
    while let Some(start) = remaining.find("<Contents>") {
        remaining = &remaining[start + "<Contents>".len()..];
        let Some(end) = remaining.find("</Contents>") else {
            return Err("S3 listing contains an unterminated Contents element".to_owned());
        };
        let contents = &remaining[..end];
        let key =
            tag_text(contents, "Key").ok_or_else(|| "S3 listing object omitted Key".to_owned())?;
        let size = tag_text(contents, "Size")
            .ok_or_else(|| "S3 listing object omitted Size".to_owned())?
            .parse::<u64>()
            .map_err(|error| format!("S3 listing object has invalid Size: {error}"))?;
        objects.push(BackupObjectEntry { key, size });
        remaining = &remaining[end + "</Contents>".len()..];
    }
    Ok(objects)
}

fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let start = xml.find(&start_tag)? + start_tag.len();
    let end = xml[start..].find(&end_tag)? + start;
    Some(xml_unescape(&xml[start..end]))
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn delete_objects_xml(keys: &[String]) -> String {
    let mut xml = String::from("<Delete xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    for key in keys {
        write!(xml, "<Object><Key>{}</Key></Object>", xml_escape(key))
            .expect("writing to a string is infallible");
    }
    xml.push_str("<Quiet>true</Quiet></Delete>");
    xml
}

fn delete_result_error(xml: &str) -> Option<String> {
    let start = xml.find("<Error>")?;
    let end = xml[start..].find("</Error>")? + start + "</Error>".len();
    let error = &xml[start..end];
    let key = tag_text(error, "Key").unwrap_or_else(|| "unknown".to_owned());
    let code = tag_text(error, "Code").unwrap_or_else(|| "Unknown".to_owned());
    let message = tag_text(error, "Message").unwrap_or_else(|| "delete failed".to_owned());
    Some(format!("S3 could not delete `{key}` ({code}): {message}"))
}

fn aws_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(b"0123456789ABCDEF"[usize::from(byte >> 4)]));
            encoded.push(char::from(b"0123456789ABCDEF"[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

fn canonical_s3_path(path: &str) -> String {
    path.replace('$', "%24")
}

fn sha256_hex(value: &[u8]) -> String {
    hex(&Sha256::digest(value))
}

fn hmac(key: &[u8], value: &[u8]) -> Result<Vec<u8>, String> {
    let mut mac = HmacSha256::new_from_slice(key).map_err(|error| error.to_string())?;
    mac.update(value);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn hex(value: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn cloud_neutral_credentials_use_only_instant_names() {
        let values = std::collections::HashMap::from([
            ("INSTANT_S3_ACCESS_KEY_ID", "neutral-access"),
            ("INSTANT_S3_SECRET_ACCESS_KEY", "neutral-secret"),
            ("AWS_ACCESS_KEY_ID", "legacy-access"),
            ("AWS_SECRET_ACCESS_KEY", "legacy-secret"),
        ]);
        let credentials =
            static_s3_credentials_from_lookup(|name| values.get(name).map(ToString::to_string))
                .unwrap();
        let S3CredentialSource::Static {
            access_key,
            secret_key,
            session_token,
        } = credentials;
        assert_eq!(access_key, "neutral-access");
        assert_eq!(secret_key, "neutral-secret");
        assert_eq!(session_token, None);

        let Err(error) = static_s3_credentials_from_lookup(|name| match name {
            "AWS_ACCESS_KEY_ID" => Some("legacy-access".to_owned()),
            "AWS_SECRET_ACCESS_KEY" => Some("legacy-secret".to_owned()),
            "AWS_SESSION_TOKEN" => Some("legacy-session".to_owned()),
            _ => None,
        }) else {
            panic!("ambient AWS credentials must be ignored")
        };
        assert_eq!(
            error,
            "INSTANT_S3_ACCESS_KEY_ID is required for S3-compatible storage"
        );
    }

    #[test]
    fn cloud_neutral_credentials_never_mix_instant_and_legacy_families() {
        for values in [
            std::collections::HashMap::from([
                ("INSTANT_S3_ACCESS_KEY_ID", "r2-access"),
                ("AWS_SECRET_ACCESS_KEY", "legacy-secret"),
            ]),
            std::collections::HashMap::from([
                ("INSTANT_S3_SECRET_ACCESS_KEY", "r2-secret"),
                ("AWS_ACCESS_KEY_ID", "legacy-access"),
            ]),
        ] {
            let Err(error) =
                static_s3_credentials_from_lookup(|name| values.get(name).map(ToString::to_string))
            else {
                panic!("partial cloud-neutral credentials must fail closed")
            };
            assert_eq!(
                error,
                "INSTANT_S3_ACCESS_KEY_ID and INSTANT_S3_SECRET_ACCESS_KEY must be set together"
            );
        }
    }

    #[tokio::test]
    async fn configured_factory_requires_an_explicit_provider_endpoint() {
        let config = crate::ServerConfig::from_pairs(Vec::<(String, String)>::new()).unwrap();
        let Err(error) = s3_object_store_from_config(&config, None).await else {
            panic!("provider-neutral builds must not infer an AWS endpoint")
        };
        assert!(error.contains("S3_ENDPOINT is required"));
        assert!(error.contains("R2 or MinIO"));
    }

    fn store(path_style: bool) -> S3ObjectStore {
        S3ObjectStore::new(S3ObjectStoreConfig {
            endpoint: "https://s3.us-east-1.amazonaws.com".to_owned(),
            download_endpoint: None,
            region: "us-east-1".to_owned(),
            bucket: "instant-files".to_owned(),
            access_key: "AKIDEXAMPLE".to_owned(),
            secret_key: "secret".to_owned(),
            session_token: None,
            force_path_style: path_style,
            download_expiry_seconds: 600,
        })
        .unwrap()
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn live_s3_put_get_delete_round_trip_when_configured() {
        let Ok(endpoint) = std::env::var("INSTANT_LIVE_S3_ENDPOINT") else {
            return;
        };
        let region =
            std::env::var("INSTANT_LIVE_S3_REGION").unwrap_or_else(|_| "us-east-1".to_owned());
        let access_key = std::env::var("INSTANT_LIVE_S3_ACCESS_KEY")
            .unwrap_or_else(|_| "instanttest".to_owned());
        let secret_key = std::env::var("INSTANT_LIVE_S3_SECRET_KEY")
            .unwrap_or_else(|_| "instanttestsecret".to_owned());
        let session_token = std::env::var("INSTANT_LIVE_S3_SESSION_TOKEN").ok();
        let bucket = std::env::var("INSTANT_LIVE_S3_BUCKET")
            .unwrap_or_else(|_| "instant-stream-tests".to_owned());
        let build_store = |bucket: String| {
            S3ObjectStore::new(S3ObjectStoreConfig {
                endpoint: endpoint.clone(),
                download_endpoint: None,
                region: region.clone(),
                bucket,
                access_key: access_key.clone(),
                secret_key: secret_key.clone(),
                session_token: session_token.clone(),
                force_path_style: true,
                download_expiry_seconds: 600,
            })
        };
        let store = build_store(bucket).unwrap();
        let secondary = std::env::var("INSTANT_LIVE_S3_SECONDARY_BUCKET")
            .ok()
            .filter(|bucket| bucket != &store.bucket)
            .map(build_store)
            .transpose()
            .unwrap();

        let prefix = format!("instant-rust-live/{}/", uuid::Uuid::new_v4());
        let key = format!("{prefix}primary.bin");
        let extra_key = format!("{prefix}bulk-delete.bin");
        let payload = b"streamed-terminal-done";
        let contract = async {
            let body = Body::from_stream(futures_util::stream::iter([
                Ok::<_, std::io::Error>(&payload[..8]),
                Ok::<_, std::io::Error>(&payload[8..]),
            ]));
            let metadata = store
                .put(
                    StoragePutRequest {
                        key: key.clone(),
                        content_type: Some("application/x-instant-live-canary".to_owned()),
                        content_disposition: Some("attachment; filename=canary.bin".to_owned()),
                        content_encoding: None,
                        content_length: Some(payload.len() as u64),
                        tags: std::collections::BTreeMap::from([(
                            "expire".to_owned(),
                            "true".to_owned(),
                        )]),
                    },
                    body,
                )
                .await?;
            if metadata.size != payload.len() as u64
                || metadata.content_type.as_deref() != Some("application/x-instant-live-canary")
                || metadata.content_disposition.as_deref()
                    != Some("attachment; filename=canary.bin")
                || metadata.etag.is_none()
                || metadata.last_modified_ms.is_none()
            {
                return Err(format!("unexpected S3 HEAD metadata: {metadata:?}"));
            }

            // R2 rejects ordinary S3 object tags, so the production adapter
            // persists the retention marker as custom metadata. Verify the
            // exact wire header when the configured endpoint is R2.
            if is_cloudflare_r2_endpoint(&store.endpoint) {
                let url = store.object_url(&key)?;
                let headers = store
                    .signed_headers(&Method::HEAD, &url, "UNSIGNED-PAYLOAD", Utc::now(), &[])
                    .await?;
                let response = store
                    .client
                    .head(url)
                    .headers(headers)
                    .send()
                    .await
                    .map_err(|error| error.to_string())?
                    .error_for_status()
                    .map_err(|error| error.to_string())?;
                if response
                    .headers()
                    .get("x-amz-meta-instant-tags")
                    .and_then(|value| value.to_str().ok())
                    != Some("expire=true")
                {
                    return Err("R2 HEAD omitted the Instant retention metadata".to_owned());
                }
            }

            let downloaded = store
                .read_object(store.bucket.clone(), key.clone(), 1_024)
                .await?;
            if downloaded != payload {
                return Err("direct S3 GET returned different bytes".to_owned());
            }
            let signed_url = store.signed_download_url(key.clone()).await?;
            let unsigned_response = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|_| "could not build the presigned S3 GET client".to_owned())?
                .get(signed_url)
                .send()
                .await
                .map_err(|_| "presigned S3 GET request failed".to_owned())?;
            if !unsigned_response.status().is_success() {
                return Err(format!(
                    "presigned S3 GET returned HTTP {}",
                    unsigned_response.status()
                ));
            }
            let unsigned_download = unsigned_response
                .bytes()
                .await
                .map_err(|_| "presigned S3 GET body failed".to_owned())?;
            if unsigned_download.as_ref() != payload {
                return Err("presigned S3 GET returned different bytes".to_owned());
            }

            let listed = store.list_backup_objects(prefix.clone()).await?;
            if !listed
                .iter()
                .any(|entry| entry.key == key && entry.size == payload.len() as u64)
            {
                return Err("ListObjectsV2 omitted the uploaded canary".to_owned());
            }

            if let Some(secondary) = &secondary {
                store
                    .copy_to_bucket(key.clone(), secondary.bucket.clone())
                    .await?;
                let copied = secondary
                    .read_backup_object(key.clone(), 1_024)
                    .await?
                    .ok_or_else(|| "cross-bucket copy was not readable".to_owned())?;
                if copied != payload {
                    return Err("cross-bucket copy returned different bytes".to_owned());
                }
                let stream = secondary
                    .stream_backup_object(key.clone())
                    .await?
                    .ok_or_else(|| "copied backup stream was missing".to_owned())?;
                let streamed = axum::body::to_bytes(stream, 1_024)
                    .await
                    .map_err(|error| error.to_string())?;
                if streamed.as_ref() != payload {
                    return Err("streamed backup read returned different bytes".to_owned());
                }
                let secondary_listing = secondary.list_backup_objects(prefix.clone()).await?;
                if !secondary_listing
                    .iter()
                    .any(|entry| entry.key == key && entry.size == payload.len() as u64)
                {
                    return Err("destination listing omitted the copied canary".to_owned());
                }
            } else {
                let stream = store
                    .stream_backup_object(key.clone())
                    .await?
                    .ok_or_else(|| "backup stream was missing".to_owned())?;
                let streamed = axum::body::to_bytes(stream, 1_024)
                    .await
                    .map_err(|error| error.to_string())?;
                if streamed.as_ref() != payload {
                    return Err("streamed backup read returned different bytes".to_owned());
                }
            }

            store
                .put(
                    StoragePutRequest {
                        key: extra_key.clone(),
                        content_type: Some("application/octet-stream".to_owned()),
                        content_disposition: None,
                        content_encoding: None,
                        content_length: Some(4),
                        tags: std::collections::BTreeMap::new(),
                    },
                    Body::from("bulk"),
                )
                .await?;
            store
                .delete_many(vec![key.clone(), extra_key.clone()])
                .await?;
            if store
                .read_backup_object(key.clone(), 1_024)
                .await?
                .is_some()
                || store
                    .read_backup_object(extra_key.clone(), 1_024)
                    .await?
                    .is_some()
            {
                return Err("bulk-deleted source object remained readable".to_owned());
            }
            if let Some(secondary) = &secondary {
                secondary.delete_many(vec![key.clone()]).await?;
                if secondary
                    .read_backup_object(key.clone(), 1_024)
                    .await?
                    .is_some()
                {
                    return Err("deleted destination object remained readable".to_owned());
                }
            }
            Ok(())
        };
        let result: Result<(), String> =
            match tokio::time::timeout(std::time::Duration::from_secs(300), contract).await {
                Ok(result) => result,
                Err(_) => Err("live S3 adapter contract exceeded five minutes".to_owned()),
            };

        // S3 DELETE is idempotent. Always attempt cleanup so an assertion or
        // transient failure cannot strand live-canary objects in either bucket.
        let mut cleanup_errors = Vec::new();
        for cleanup_key in [&key, &extra_key] {
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                store.delete(cleanup_key.clone()),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    cleanup_errors.push(format!("{}:{cleanup_key}: {error}", store.bucket));
                }
                Err(_) => cleanup_errors.push(format!(
                    "{}:{cleanup_key}: cleanup exceeded 30 seconds",
                    store.bucket
                )),
            }
        }
        if let Some(secondary) = &secondary {
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                secondary.delete(key.clone()),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    cleanup_errors.push(format!("{}:{key}: {error}", secondary.bucket));
                }
                Err(_) => cleanup_errors.push(format!(
                    "{}:{key}: cleanup exceeded 30 seconds",
                    secondary.bucket
                )),
            }
        }
        if let Err(error) = result {
            panic!("live S3 adapter contract failed: {error}; cleanup: {cleanup_errors:?}");
        }
        assert!(
            cleanup_errors.is_empty(),
            "live S3 adapter cleanup failed: {cleanup_errors:?}"
        );
    }

    #[test]
    fn object_urls_support_path_and_virtual_host_s3_endpoints() {
        assert_eq!(
            store(true).object_url("app/1/file").unwrap().as_str(),
            "https://s3.us-east-1.amazonaws.com/instant-files/app/1/file"
        );
        assert_eq!(
            store(false).object_url("app/1/file").unwrap().as_str(),
            "https://instant-files.s3.us-east-1.amazonaws.com/app/1/file"
        );
        assert!(
            store(true)
                .object_url("app/1/entities/$files.jsonl")
                .is_ok()
        );
        assert_eq!(
            canonical_s3_path("/instant-files/app/1/entities/$files.jsonl"),
            "/instant-files/app/1/entities/%24files.jsonl"
        );
        for key in ["../secret", "app/../secret", "app/./secret", "/secret"] {
            assert!(store(true).object_url(key).is_err(), "accepted {key}");
        }
    }

    #[test]
    fn list_objects_xml_parser_preserves_keys_sizes_and_entities() {
        let xml = "<ListBucketResult><IsTruncated>false</IsTruncated>\
                   <Contents><Key>app/config.json</Key><Size>42</Size></Contents>\
                   <Contents><Key>app/a&amp;b.jsonl</Key><Size>7</Size></Contents>\
                   </ListBucketResult>";
        assert_eq!(
            parse_list_objects(xml).unwrap(),
            vec![
                BackupObjectEntry {
                    key: "app/config.json".to_owned(),
                    size: 42,
                },
                BackupObjectEntry {
                    key: "app/a&b.jsonl".to_owned(),
                    size: 7,
                },
            ]
        );
    }

    #[test]
    fn delete_objects_xml_is_quiet_escaped_and_surfaces_per_key_errors() {
        assert_eq!(
            delete_objects_xml(&["app/a&b".to_owned(), "app/<c>".to_owned()]),
            "<Delete xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Object><Key>app/a&amp;b</Key></Object><Object><Key>app/&lt;c&gt;</Key></Object><Quiet>true</Quiet></Delete>"
        );
        assert_eq!(delete_result_error("<DeleteResult/>"), None);
        assert_eq!(
            delete_result_error(
                "<DeleteResult><Error><Key>app/a&amp;b</Key><Code>AccessDenied</Code><Message>denied</Message></Error></DeleteResult>"
            ),
            Some("S3 could not delete `app/a&b` (AccessDenied): denied".to_owned())
        );
    }

    #[test]
    fn r2_tagged_uploads_use_supported_custom_metadata() {
        let tags = std::collections::BTreeMap::from([
            ("expire".to_owned(), "true".to_owned()),
            ("policy".to_owned(), "nightly backup".to_owned()),
        ]);
        assert_eq!(
            store(true).upload_tag_headers(&tags),
            vec![(
                "x-amz-tagging".to_owned(),
                "expire=true&policy=nightly+backup".to_owned(),
            )]
        );

        let r2 = S3ObjectStore::new(S3ObjectStoreConfig {
            endpoint: "https://account.eu.r2.cloudflarestorage.com".to_owned(),
            download_endpoint: None,
            region: "auto".to_owned(),
            bucket: "instantdb-backups".to_owned(),
            access_key: "access".to_owned(),
            secret_key: "secret".to_owned(),
            session_token: None,
            force_path_style: true,
            download_expiry_seconds: 600,
        })
        .unwrap();
        assert_eq!(
            r2.upload_tag_headers(&tags),
            vec![(
                "x-amz-meta-instant-tags".to_owned(),
                "expire=true&policy=nightly+backup".to_owned(),
            )]
        );
        assert!(
            r2.upload_tag_headers(&std::collections::BTreeMap::new())
                .is_empty()
        );
    }

    #[tokio::test]
    async fn presigning_is_deterministic_encoded_and_never_exposes_the_secret() {
        assert_eq!(aws_encode("a b/+"), "a%20b%2F%2B");
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap();
        let url = store(true).presigned_url("app/1/file", now).await.unwrap();
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        assert!(
            url.contains("X-Amz-Credential=AKIDEXAMPLE%2F20260827%2Fus-east-1%2Fs3%2Faws4_request")
        );
        assert!(url.contains("X-Amz-Expires=600"));
        assert!(url.contains("X-Amz-Signature="));
        assert!(!url.contains("secret"));
    }

    #[tokio::test]
    async fn signing_a_batch_never_reads_object_metadata() {
        // Nothing listens on this endpoint, so any HEAD issued while signing
        // would fail the call. Returning a full batch proves that signing is
        // pure computation and never reads per-object metadata.
        let store = S3ObjectStore::new(S3ObjectStoreConfig {
            endpoint: "http://127.0.0.1:9".to_owned(),
            download_endpoint: None,
            region: "us-east-1".to_owned(),
            bucket: "instant-files".to_owned(),
            access_key: "AKIDEXAMPLE".to_owned(),
            secret_key: "secret".to_owned(),
            session_token: None,
            force_path_style: true,
            download_expiry_seconds: 600,
        })
        .unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap();
        let keys = (0..8)
            .map(|index| format!("app/1/file-{index}"))
            .collect::<Vec<_>>();
        let urls = store
            .signed_download_urls_for_app_at(uuid::Uuid::nil(), keys.clone(), now)
            .await
            .unwrap();
        assert_eq!(urls.len(), keys.len());
        for (url, key) in urls.iter().zip(&keys) {
            assert!(url.contains(key.as_str()), "{url}");
            assert!(!url.contains("response-content-disposition"), "{url}");
        }
    }

    #[tokio::test]
    async fn signed_headers_include_the_canonical_header_separator() {
        let store = S3ObjectStore::new(S3ObjectStoreConfig {
            endpoint: "http://127.0.0.1:9000".to_owned(),
            download_endpoint: None,
            region: "us-east-1".to_owned(),
            bucket: "instant-stream-tests".to_owned(),
            access_key: "instanttest".to_owned(),
            secret_key: "instanttestsecret".to_owned(),
            session_token: None,
            force_path_style: true,
            download_expiry_seconds: 600,
        })
        .unwrap();
        let url = store
            .object_url(
                "4874c8d2-8c94-4c70-b5da-1cedf51f712e/5/\
                 e45eea09-8725-4143-a3e2-ce9d6cf8e0c0",
            )
            .unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 29, 6, 58, 20).unwrap();
        let headers = store
            .signed_headers(&Method::PUT, &url, "UNSIGNED-PAYLOAD", now, &[])
            .await
            .unwrap();

        assert_eq!(
            headers[AUTHORIZATION].to_str().unwrap(),
            "AWS4-HMAC-SHA256 \
             Credential=instanttest/20260829/us-east-1/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date, \
             Signature=df88776059184041d1d4f1d924e0b2ac0279f1fcbbe9b2c7d6a9fadd9b1c2be5"
        );
    }

    #[tokio::test]
    async fn public_download_endpoint_does_not_change_request_endpoint() {
        let store = S3ObjectStore::new(S3ObjectStoreConfig {
            endpoint: "http://minio.internal:9000".to_owned(),
            download_endpoint: Some("https://files.example.test".to_owned()),
            region: "us-east-1".to_owned(),
            bucket: "instant-files".to_owned(),
            access_key: "AKIDEXAMPLE".to_owned(),
            secret_key: "secret".to_owned(),
            session_token: None,
            force_path_style: true,
            download_expiry_seconds: 600,
        })
        .unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap();

        assert_eq!(
            store.object_url("app/1/file").unwrap().as_str(),
            "http://minio.internal:9000/instant-files/app/1/file"
        );
        assert!(
            store
                .presigned_url("app/1/file", now)
                .await
                .unwrap()
                .starts_with("https://files.example.test/instant-files/app/1/file?")
        );
    }

    #[tokio::test]
    async fn upstream_2872_virtual_host_public_endpoint_presigns_without_bucket_path() {
        let config = crate::ServerConfig::from_pairs([
            ("S3_ENDPOINT", "https://s3.internal.test"),
            ("S3_PUBLIC_ENDPOINT", "https://s3.public.test"),
            ("INSTANT_S3_REGION", "auto"),
            ("S3_BUCKET", "instant-files"),
            ("S3_VIRTUAL_HOST_STYLE", "true"),
        ])
        .unwrap();
        let credentials = S3CredentialSource::Static {
            access_key: "AKIDEXAMPLE".to_owned(),
            secret_key: "secret".to_owned(),
            session_token: None,
        };
        let store = S3ObjectStore::build(
            configured_runtime(&config, "instant-files").unwrap(),
            credentials.clone(),
            credentials,
        )
        .unwrap();
        assert_eq!(
            store.object_url("app/$files.jsonl").unwrap().as_str(),
            "https://instant-files.s3.internal.test/app/$files.jsonl"
        );
        let signed = store
            .presigned_url("app/$files.jsonl", Utc::now())
            .await
            .unwrap();
        assert!(signed.starts_with("https://instant-files.s3.public.test/app/$files.jsonl?"));
        assert!(signed.contains("X-Amz-Signature="));
    }

    #[tokio::test]
    async fn presigning_matches_the_aws_s3_sigv4_reference_vector() {
        let store = S3ObjectStore::new(S3ObjectStoreConfig {
            endpoint: "https://s3.amazonaws.com".to_owned(),
            download_endpoint: None,
            region: "us-east-1".to_owned(),
            bucket: "examplebucket".to_owned(),
            access_key: "AKIAIOSFODNN7EXAMPLE".to_owned(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_owned(),
            session_token: None,
            force_path_style: false,
            download_expiry_seconds: 86_400,
        })
        .unwrap();
        let now = Utc.with_ymd_and_hms(2013, 5, 24, 0, 0, 0).unwrap();

        assert_eq!(
            store.presigned_url("test.txt", now).await.unwrap(),
            concat!(
                "https://examplebucket.s3.amazonaws.com/test.txt?",
                "X-Amz-Algorithm=AWS4-HMAC-SHA256&",
                "X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&",
                "X-Amz-Date=20130524T000000Z&",
                "X-Amz-Expires=86400&",
                "X-Amz-SignedHeaders=host&",
                "X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
            )
        );
    }

    #[test]
    fn invalid_configuration_and_object_keys_fail_closed() {
        assert!(
            S3ObjectStore::new(S3ObjectStoreConfig {
                endpoint: "file:///tmp/s3".to_owned(),
                download_endpoint: None,
                region: "us-east-1".to_owned(),
                bucket: "bucket".to_owned(),
                access_key: "key".to_owned(),
                secret_key: "secret".to_owned(),
                session_token: None,
                force_path_style: true,
                download_expiry_seconds: 600,
            })
            .is_err()
        );
        assert!(store(true).object_url("unsafe key").is_err());
    }
}
