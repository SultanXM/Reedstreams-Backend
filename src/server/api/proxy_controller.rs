// these are pretty basic scripts and won't be used anywhere else so it's not worth starting them
// as a service due to how independent they are
use axum::{
    Router,
    extract::Query,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use std::io::{Read, Write};

use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::Deserialize;
use tracing::{debug, error, info};

/// Supported compression encodings
#[derive(Debug, Clone, Copy, PartialEq)]
enum ContentEncoding {
    Zstd,
    Gzip,
    None,
}

impl ContentEncoding {
    /// determine the best encoding based on Accept-Encoding header
    /// apple HLS player sends "gzip, deflate" or "identity" - IT MUST BE RESPECTED (i think)
    ///
    /// this is a work in progress. Current issues arise from content-length missing? HAR files
    /// show that the client doesn't recieve them and doesn't query for any more m3u8s for some
    /// reason. Not sure what the issue is, please help me on this if you read it before I remove
    /// this comment LMAO
    fn from_accept_encoding(accept_encoding: Option<&str>) -> Self {
        match accept_encoding {
            Some(v) => {
                // don't compress if client explicitly requests identity-only
                if v == "identity" || v.starts_with("identity,") {
                    return Self::None;
                }
                // Prefer zstd if supported (better compression), fallback to gzip
                if v.contains("zstd") {
                    Self::Zstd
                } else if v.contains("gzip") {
                    Self::Gzip
                } else {
                    Self::None
                }
            }
            None => Self::None,
        }
    }

    fn as_header_value(&self) -> Option<&'static str> {
        match self {
            Self::Zstd => Some("zstd"),
            Self::Gzip => Some("gzip"),
            Self::None => None,
        }
    }

    fn compress(&self, data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
        match self {
            Self::Zstd => zstd::encode_all(data, 3),
            Self::Gzip => {
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(data)?;
                encoder.finish()
            }
            Self::None => Ok(data.to_vec()),
        }
    }
}

use crate::server::{
    error::{AppResult, Error},
    extractors::EdgeAuthentication,
    services::{cookie_services::CookieService, edge_services::EdgeServices},
    utils::signature_utils::SignatureUtil,
};

#[derive(Deserialize)]
struct ProxyQuery {
    url: String,
    schema: Option<String>,
}

pub struct ProxyController;

impl ProxyController {
    pub fn app() -> Router {
        Router::new().route("/", get(Self::proxy_get).options(Self::proxy_options))
        // this is a movie specific route
        // .route("/captions", get(Self::proxy_captions))
    }

    /// build m3u8 response with proper headers and optional compression
    fn build_m3u8_response(processed_body: &str, headers: &HeaderMap) -> AppResult<Response> {
        // determine client's preferred encoding (apple hls likes gzip, not zstd)
        let encoding = ContentEncoding::from_accept_encoding(
            headers
                .get(header::ACCEPT_ENCODING)
                .and_then(|v| v.to_str().ok()),
        );

        let mut response_headers = HeaderMap::new();
        response_headers.insert(
            header::CONTENT_TYPE,
            "application/vnd.apple.mpegurl"
                .parse()
                .expect("Static header value should parse"),
        );
        response_headers.insert(
            header::CACHE_CONTROL,
            "no-cache"
                .parse()
                .expect("Static header value should parse"),
        );

        let response_body: Vec<u8> = if encoding != ContentEncoding::None {
            let compressed_body = encoding.compress(processed_body.as_bytes()).map_err(|e| {
                error!("Failed to compress response with {:?}: {}", encoding, e);
                Error::InternalServerErrorWithContext("Failed to compress response".to_string())
            })?;
            debug!(
                "Compressed M3U8 with {:?} from {} to {} bytes",
                encoding,
                processed_body.len(),
                compressed_body.len()
            );
            if let Some(enc_header) = encoding.as_header_value() {
                response_headers.insert(
                    header::CONTENT_ENCODING,
                    enc_header
                        .parse()
                        .expect("Static header value should parse"),
                );
            }
            compressed_body
        } else {
            debug!(
                "Client doesn't accept compression, sending uncompressed M3U8 {} bytes",
                processed_body.len()
            );
            processed_body.as_bytes().to_vec()
        };

        response_headers.insert(
            header::CONTENT_LENGTH,
            response_body
                .len()
                .to_string()
                .parse()
                .expect("Content length should parse"),
        );

        Ok((StatusCode::OK, response_headers, response_body).into_response())
    }

    async fn proxy_get(
        EdgeAuthentication(client_id, services): EdgeAuthentication,
        Query(params): Query<ProxyQuery>,
        headers: HeaderMap,
    ) -> AppResult<Response> {
        let target_url = Self::decode_url(&params.url)?;

        if !target_url.starts_with("http://") && !target_url.starts_with("https://") {
            return Err(Error::BadRequest("Invalid URL format".to_string()));
        }

        let schema = params.schema.as_deref().unwrap_or("sports");
        debug!("Proxying (schema={}): {}", schema, target_url);

        // extract domain for cookie handling
        let domain = CookieService::extract_domain(&target_url);

        // load any stored cookies for this domain
        let stored_cookies = if let Some(ref d) = domain {
            services.cookies.get_cookies(d).await
        } else {
            None
        };

        let client = reqwest::Client::new();
        let mut request_builder =
            Self::apply_schema_headers(client.get(&target_url), schema, &target_url, &headers);

        // add cookies to request
        if let Some(cookies) = stored_cookies {
            debug!("Adding stored cookies to request: {}", cookies);
            request_builder = request_builder.header(header::COOKIE, cookies);
        }

        debug!("Sending request to target");

        let target_response = request_builder.send().await.map_err(|e| {
            error!("Request failed: {}", e);
            // record error for rate limiting - spawn to not block the response
            let rate_limit = services.rate_limit.clone();
            let uid = client_id.clone();

            // spawn a new thread to handle this, it's not relevant to this
            tokio::spawn(async move {
                rate_limit.record_error(&uid, "proxy_request_failed").await;
            });
            Error::InternalServerErrorWithContext(format!("Request failed: {}", e))
        })?;

        debug!(
            "Received response with status: {}",
            target_response.status()
        );

        // store cookies
        if let Some(ref d) = domain {
            let set_cookies: Vec<String> = target_response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok().map(|s| s.to_string()))
                .collect();

            if !set_cookies.is_empty() {
                debug!("Storing {} cookies from response", set_cookies.len());
                let cookie_service = services.cookies.clone();
                let domain_clone = d.clone();
                tokio::spawn(async move {
                    cookie_service
                        .store_cookies(&domain_clone, &set_cookies)
                        .await;
                });
            }
        }

        // this line WILL get hit at some point.
        let response_status = target_response.status();
        if !response_status.is_success() {
            let _target_bytes = target_response.bytes().await.map_or_else(
                |_| "No response".to_string(),
                |b| {
                    String::from_utf8(b.to_vec())
                        .unwrap_or_else(|_| "Non-UTF8 response".to_string())
                },
            );
            // here is where it's up to you, I don't like to print the whole target_bytes as it's
            // often a cloudflare html page that clogs everything, but if wanted, the target bytes
            // are right above
            error!(
                "User: {}, Response from target not successful: {}",
                client_id, response_status
            );
            // Record error for rate limiting - these upstream errors count against the user
            // only if they're client-induced (4xx) not server errors (5xx)
            if response_status.is_client_error() {
                let rate_limit = services.rate_limit.clone();
                let uid = client_id.clone();
                tokio::spawn(async move {
                    rate_limit
                        .record_error(&uid, "proxy_upstream_client_error")
                        .await;
                });
            }
            return Err(Error::BadRequest(
                "Api returned an invalid response".to_string(),
            ));

            // if you want, you can wait around 400ms and then send this retryy that mimcs what the
            // browser would do. Sometimes it skips having the client resend the request.
            //
            // let retry_result =
            //     Self::retry_request_with_te_trailers(&client, &target_url, schema, &headers).await;

            // match retry_result {
            //     Ok(response) if response.status().is_success() => {
            //         info!("Retry with TE: trailers succeeded");
            //         target_response = response;
            //     }
            //     _ => {
            //         return Err((
            //             StatusCode::INTERNAL_SERVER_ERROR,
            //             format!("Received fatal status code: {}", response_status),
            //         ));
            //     }
            // }
        }

        let content_type = target_response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let content_encoding = target_response
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let is_mp4 = content_type.contains("video/mp4");
        debug!(
            "Content-Type: {}, Encoding: {:?}, Is MP4: {}",
            content_type, content_encoding, is_mp4
        );

        debug!("Reading response bytes");
        let bytes = target_response.bytes().await.map_err(|e| {
            error!("Failed to read response: {}", e);
            Error::InternalServerErrorWithContext(format!("Failed to read response: {}", e))
        })?;
        debug!("Read {} bytes", bytes.len());

        let decompressed: Vec<u8> = match content_encoding.as_deref() {
            Some("zstd") => {
                debug!("Decompressing zstd-encoded response");
                zstd::decode_all(&bytes[..]).map_err(|e| {
                    error!("Failed to decompress zstd: {}", e);
                    Error::InternalServerErrorWithContext(
                        "Failed to decompress response".to_string(),
                    )
                })?
            }
            Some("gzip") => {
                debug!("Decompressing gzip-encoded response");
                let mut decoder = GzDecoder::new(&bytes[..]);
                let mut decomp: Vec<u8> = Vec::new();
                decoder.read_to_end(&mut decomp).map_err(|e| {
                    error!("Failed to decompress gzip response: {}", e);
                    Error::InternalServerErrorWithContext(
                        "Failed to decompress response".to_string(),
                    )
                })?;
                decomp
            }
            _ => bytes.to_vec(),
        };

        debug!("Decompressed size: {} bytes", decompressed.len());

        // check if content starts with #EXT to detect M3U8, or default to M3U8 unless MP4
        let is_m3u8 = if is_mp4 {
            false
        } else {
            decompressed.starts_with(b"#EXT")
                || content_type.contains("mpegurl")
                || content_type.contains("m3u8")
        };
        debug!("Detected as M3U8: {}, MP4: {}", is_m3u8, is_mp4);

        if is_m3u8 {
            debug!("Processing as M3U8 playlist");
            let text = String::from_utf8(decompressed).map_err(|e| {
                error!("Failed to parse m3u8 as UTF-8: {}", e);
                Error::InternalServerErrorWithContext("Invalid m3u8 encoding".to_string())
            })?;
            debug!("M3U8 text length: {} chars", text.len());

            let processed_body = Self::process_m3u8_by_schema_with_retry(
                &text,
                &target_url,
                &client_id,
                &services,
                schema,
            )?;
            debug!(
                "Processed M3U8, response length: {} bytes",
                processed_body.len()
            );

            Ok(Self::build_m3u8_response(&processed_body, &headers)?)
        } else {
            let full_bytes = decompressed;
            let total_len = full_bytes.len();

            // this is loop hell
            let (response_bytes, status_code, range_header) = if let Some(range_value) =
                headers.get(header::RANGE)
            {
                if let Ok(range_str) = range_value.to_str() {
                    // parse "bytes=start-end" format
                    if let Some(range_part) = range_str.strip_prefix("bytes=") {
                        let parts: Vec<&str> = range_part.split('-').collect();
                        if parts.len() == 2 {
                            let start: usize = parts[0].parse().unwrap_or(0);
                            let end: usize = if parts[1].is_empty() {
                                total_len.saturating_sub(1)
                            } else {
                                parts[1].parse().unwrap_or(total_len.saturating_sub(1))
                            };
                            let end = end.min(total_len.saturating_sub(1));

                            if start < total_len && start <= end {
                                let sliced = full_bytes[start..=end].to_vec();
                                let content_range =
                                    format!("bytes {}-{}/{}", start, end, total_len);
                                debug!("Serving range {}-{} of {} bytes", start, end, total_len);
                                (sliced, StatusCode::PARTIAL_CONTENT, Some(content_range))
                            } else {
                                (full_bytes, StatusCode::OK, None)
                            }
                        } else {
                            (full_bytes, StatusCode::OK, None)
                        }
                    } else {
                        (full_bytes, StatusCode::OK, None)
                    }
                } else {
                    (full_bytes, StatusCode::OK, None)
                }
            } else {
                (full_bytes, StatusCode::OK, None)
            };

            // determine client's preferred encoding
            let encoding = ContentEncoding::from_accept_encoding(
                headers
                    .get(header::ACCEPT_ENCODING)
                    .and_then(|v| v.to_str().ok()),
            );

            let mut response_headers = HeaderMap::new();

            response_headers.insert(
                header::CONTENT_TYPE,
                "video/mp2t"
                    .parse()
                    .expect("Static header value should parse"),
            );

            let cache_control = if is_mp4 {
                "public, max-age=3600"
            } else {
                "public, max-age=31536000"
            };

            response_headers.insert(
                header::CACHE_CONTROL,
                cache_control
                    .parse()
                    .expect("Static header value should parse"),
            );

            // indicate we accept ranges
            response_headers.insert(
                header::ACCEPT_RANGES,
                "bytes".parse().expect("Static header value should parse"),
            );

            // Add Content-Range header if this is a range response
            if let Some(range_val) = range_header {
                response_headers.insert(
                    header::CONTENT_RANGE,
                    range_val.parse().expect("Range header should parse"),
                );
            }

            // only compress full responses
            let final_bytes = if encoding != ContentEncoding::None
                && status_code != StatusCode::PARTIAL_CONTENT
            {
                let compressed_bytes = encoding.compress(&response_bytes).map_err(|e| {
                    error!(
                        "Failed to compress binary response with {:?}: {}",
                        encoding, e
                    );
                    Error::InternalServerErrorWithContext("Failed to compress response".to_string())
                })?;
                debug!(
                    "Compressed binary with {:?} from {} to {} bytes",
                    encoding,
                    response_bytes.len(),
                    compressed_bytes.len()
                );
                if let Some(enc_header) = encoding.as_header_value() {
                    response_headers.insert(
                        header::CONTENT_ENCODING,
                        enc_header
                            .parse()
                            .expect("Static header value should parse"),
                    );
                }
                compressed_bytes
            } else {
                debug!(
                    "Sending uncompressed {} bytes (partial: {})",
                    response_bytes.len(),
                    status_code == StatusCode::PARTIAL_CONTENT
                );
                response_bytes
            };

            response_headers.insert(
                header::CONTENT_LENGTH,
                final_bytes
                    .len()
                    .to_string()
                    .parse()
                    .expect("Content length should parse"),
            );

            Ok((status_code, response_headers, final_bytes).into_response())
        }
    }

    async fn proxy_options() -> impl IntoResponse {
        StatusCode::NO_CONTENT
    }

    // function that sometimes fixed issues that i had above
    //
    // async fn retry_request_with_te_trailers(
    //     client: &reqwest::Client,
    //     target_url: &str,
    //     schema: &str,
    //     headers: &HeaderMap,
    // ) -> Result<reqwest::Response, reqwest::Error> {
    //     debug!("Retrying request with TE: trailers header");
    //     let request_builder =
    //         Self::apply_schema_headers(client.get(target_url), schema, target_url, headers);
    //     let request_builder = request_builder.header("TE", "trailers");
    //     request_builder.send().await
    // }

    // keeping this here in case this somehow is every needed to be used for sports streaming, I
    // haven't found a use for this other than in movies though
    // async fn proxy_captions(Query(params): Query<ProxyQuery>) -> AppResult<Response> {
    //     let target_url = Self::decode_url(&params.url)?;

    //     if !target_url.starts_with("http://") && !target_url.starts_with("https://") {
    //         return Err(Error::BadRequest("Invalid URL format".to_string()));
    //     }

    //     debug!("Proxying caption: {}", target_url);

    //     let client = reqwest::Client::new();
    //     let request_builder = client
    //         .get(&target_url)
    //         .header(
    //             header::USER_AGENT,
    //             "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:145.0) Gecko/20100101 Firefox/145.0",
    //         )
    //         .header(header::ACCEPT, "*/*");

    //     let target_response = request_builder.send().await.map_err(|e| {
    //         error!("Caption request failed: {}", e);
    //         Error::InternalServerErrorWithContext(format!("Caption request failed: {}", e))
    //     })?;

    //     if !target_response.status().is_success() {
    //         error!("Caption fetch failed: {}", target_response.status());
    //         return Err(Error::BadRequest(format!(
    //             "Failed to fetch caption: {}",
    //             target_response.status()
    //         )));
    //     }

    //     let content_type = target_response
    //         .headers()
    //         .get(header::CONTENT_TYPE)
    //         .and_then(|v| v.to_str().ok())
    //         .unwrap_or("text/vtt")
    //         .to_string();

    //     let bytes = target_response.bytes().await.map_err(|e| {
    //         error!("Failed to read caption response: {}", e);
    //         Error::InternalServerErrorWithContext(format!("Failed to read caption response: {}", e))
    //     })?;

    //     let mut response_headers = HeaderMap::new();
    //     response_headers.insert(
    //         header::CONTENT_TYPE,
    //         content_type.parse().unwrap_or_else(|_| {
    //             "text/vtt"
    //                 .parse()
    //                 .expect("Static header value should parse")
    //         }),
    //     );
    //     response_headers.insert(
    //         header::CACHE_CONTROL,
    //         "public, max-age=86400"
    //             .parse()
    //             .expect("Static header value should parse"),
    //     );
    //     response_headers.insert(
    //         header::CONTENT_LENGTH,
    //         bytes
    //             .len()
    //             .to_string()
    //             .parse()
    //             .expect("Content length should parse"),
    //     );

    //     Ok((StatusCode::OK, response_headers, bytes).into_response())
    // }

    // decode my url encoding
    fn decode_url(url_param: &str) -> AppResult<String> {
        if url_param.starts_with("http://") || url_param.starts_with("https://") {
            urlencoding::decode(url_param)
                .map(|s| s.to_string())
                .map_err(|e| {
                    error!("Failed to decode URL: {}", e);
                    Error::BadRequest("Invalid URL encoding".to_string())
                })
        } else {
            let mut padded = url_param.to_string();
            while !padded.len().is_multiple_of(4) {
                padded.push('=');
            }

            URL_SAFE
                .decode(&padded)
                .map_err(|e| {
                    error!("Failed to decode base64: {}", e);
                    Error::BadRequest("Invalid URL encoding".to_string())
                })
                .and_then(|bytes| {
                    String::from_utf8(bytes).map_err(|e| {
                        error!("Failed to parse UTF-8: {}", e);
                        Error::BadRequest("Invalid URL encoding".to_string())
                    })
                })
        }
    }

    // this should always be sports but I'll keep it here incase you want to switch sources to
    // streamed.pk or something and want to send their headers
    fn apply_schema_headers(
        mut request_builder: reqwest::RequestBuilder,
        schema: &str,
        target_url: &str,
        _headers: &HeaderMap,
    ) -> reqwest::RequestBuilder {
        match schema {
            // not needed for this case but it's here as another example
            // "movie" => {
            //     request_builder
            //         .header(header::HOST, "storm.vodvidl.site")
            //         .header(header::ORIGIN, "https://vidlink.pro")
            //         .header(header::REFERER, "https://vidlink.pro/")
            //         .header(
            //             header::USER_AGENT,
            //             "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:145.0) Gecko/20100101 Firefox/145.0",
            //         )
            //         .header(header::ACCEPT, "*/*")
            //         .header(header::TE, "trailers")
            // }
            "sports" => {
                // Always request compressed content from upstream - we handle decompression ourselves
                // and will respect the client's Accept-Encoding when sending the response back
                let accept_encoding = "gzip, deflate, br, zstd";

                if target_url.contains("strm.poocloud.in") {
                    request_builder = request_builder
                        .header(header::ORIGIN, "https://ppvs.su")
                        .header(header::ACCEPT, "*/*")
                        .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
                        .header(header::ACCEPT_ENCODING, accept_encoding)
                        .header(header::REFERER, "https://modistreams.org/")
                        .header(
                            header::USER_AGENT,
                            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
                        )
                        .header("Sec-GPC", "1")
                        .header("Sec-Fetch-Dest", "empty")
                        .header("Sec-Fetch-Mode", "cors")
                        .header("Sec-Fetch-Site", "cross-site")
                        .header(header::CONNECTION, "keep-alive")
                        .header("Priority", "u=4")
                        .header(header::PRAGMA, "no-cache")
                        .header(header::CACHE_CONTROL, "no-cache")
                } else {
                    request_builder = request_builder
                        .header(header::REFERER, "https://api.ppvs.su/api/streams/")
                        .header(header::ORIGIN, "https://api.ppvs.su/api/streams")
                        .header(
                            header::USER_AGENT,
                            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
                        )
                        .header(header::ACCEPT_ENCODING, accept_encoding)
                        .header(header::ACCEPT, "*/*");
                }

                // forward Range headers - we fetch full content, decompress, then serve the range ourselves
                request_builder
            }
            "captions" => {
                request_builder
                    .header(
                        header::USER_AGENT,
                        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:145.0) Gecko/20100101 Firefox/145.0",
                    )
                    .header(header::ACCEPT, "*/*")
            }
            _ => {
                // default to sports if anything, but this ideally shouldn't happen and I should
                // probably just send an error because it's malicious most times but whatever it's
                // authenticated for right now.
                info!("Unknown schema, falling back to sports headers");

                // Always request compressed content from upstream
                let accept_encoding = "gzip, deflate, br, zstd";

                request_builder = request_builder
                    .header(header::REFERER, "https://api.ppvs.su/api/streams/")
                    .header(header::ORIGIN, "https://api.ppvs.su/api/streams")
                    .header(
                        header::USER_AGENT,
                        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
                    )
                    .header(header::ACCEPT_ENCODING, accept_encoding)
                    .header(header::ACCEPT, "*/*");

                // Don't forward Range headers - we fetch full content, decompress, then serve the range ourselves
                request_builder
            }
        }
    }

    fn process_m3u8_by_schema(
        text: &str,
        target_url: &str,
        client_id: &str,
        services: &EdgeServices,
        _schema: &str,
    ) -> AppResult<String> {
        // matcher for later if needed
        {
            debug!("Processing with sports schema");
            Self::process_m3u8(text, target_url, client_id, services)
        }
    }

    fn process_m3u8_by_schema_with_retry(
        text: &str,
        target_url: &str,
        client_id: &str,
        services: &EdgeServices,
        schema: &str,
    ) -> AppResult<String> {
        let result = Self::process_m3u8_by_schema(text, target_url, client_id, services, schema);

        match &result {
            Err(Error::InternalServerError | Error::InternalServerErrorWithContext(_)) => {
                error!("M3U8 processing failed with internal error, retrying once");
                // i forget why this is here, I'm leaving it because there HAS to be some reason
                // for why it's here LMAO
                //
                // I don't recall ever seeing the above error! ever triggering though so I'm not
                // sure when this would happen
                Self::process_m3u8_by_schema(text, target_url, client_id, services, schema)
            }
            _ => result,
        }
    }

    fn process_m3u8(
        text: &str,
        target_url: &str,
        client_id: &str,
        services: &EdgeServices,
    ) -> AppResult<String> {
        let base_url = url::Url::parse(target_url).map_err(|e| {
            error!("Failed to parse base URL: {}", e);
            Error::InternalServerErrorWithContext(format!("Invalid base URL: {}", e))
        })?;

        let base_path = format!(
            "{}://{}{}",
            base_url.scheme(),
            base_url.host_str().unwrap_or(""),
            &base_url.path()[..base_url.path().rfind('/').unwrap_or(0) + 1]
        );

        // trim comment lines that start with ## because it's some stupid fucking smiley face that
        // says processed by indians in a hamster wheel LMAO
        let lines: Vec<String> = text
            .lines()
            .filter(|line| !line.trim().starts_with("##"))
            .map(|line| {
                let trimmed = line.trim();

                if trimmed.is_empty() || trimmed.starts_with('#') {
                    return line.to_string();
                }

                let full_url = if trimmed.starts_with("http://") || trimmed.starts_with("https://")
                {
                    trimmed.to_string()
                } else {
                    match url::Url::parse(&base_path).and_then(|base| base.join(trimmed)) {
                        Ok(resolved) => resolved.to_string(),
                        Err(e) => {
                            error!("Failed to resolve: {} - {}", trimmed, e);
                            return line.to_string();
                        }
                    }
                };

                let encoded = URL_SAFE
                    .encode(full_url.as_bytes())
                    .trim_end_matches('=')
                    .to_string();

                let expiry = SignatureUtil::generate_expiry(12); // 12 hours
                // sign just the encoded URL to avoid path mismatch issues
                let signature = services
                    .signature_util
                    .generate_signature(client_id, expiry, &encoded);

                format!(
                    "/api/v1/proxy?url={}&schema=sports&sig={}&exp={}&client={}",
                    encoded,
                    signature,
                    expiry,
                    urlencoding::encode(client_id)
                )
            })
            .collect();

        Ok(lines.join("\n"))
    }

    // movie processing not needed, but it's another example
    // fn process_m3u8_movie(
    //     text: &str,
    //     target_url: &str,
    //     client_id: &str,
    //     services: &EdgeServices,
    // ) -> AppResult<String> {
    //     let base_url = url::Url::parse(target_url).map_err(|e| {
    //         error!("Failed to parse base URL: {}", e);
    //         Error::InternalServerErrorWithContext(format!("Invalid base URL: {}", e))
    //     })?;

    //     let base_path = format!(
    //         "{}://{}{}",
    //         base_url.scheme(),
    //         base_url.host_str().unwrap_or(""),
    //         &base_url.path()[..base_url.path().rfind('/').unwrap_or(0) + 1]
    //     );

    //     let lines: Vec<String> = text
    //         .lines()
    //         .filter(|line| !line.trim().starts_with("##"))
    //         .map(|line| {
    //             let trimmed = line.trim();

    //             if trimmed.is_empty() || trimmed.starts_with('#') {
    //                 return line.to_string();
    //             }

    //             let full_url = if trimmed.starts_with("http://") || trimmed.starts_with("https://")
    //             {
    //                 trimmed.to_string()
    //             } else if trimmed.starts_with('/') {
    //                 format!(
    //                     "{}://{}{}",
    //                     base_url.scheme(),
    //                     base_url.host_str().unwrap_or(""),
    //                     trimmed
    //                 )
    //             } else {
    //                 match url::Url::parse(&base_path).and_then(|base| base.join(trimmed)) {
    //                     Ok(resolved) => resolved.to_string(),
    //                     Err(e) => {
    //                         error!("Failed to resolve: {} - {}", trimmed, e);
    //                         return line.to_string();
    //                     }
    //                 }
    //             };

    //             let encoded = URL_SAFE
    //                 .encode(full_url.as_bytes())
    //                 .trim_end_matches('=')
    //                 .to_string();

    //             // Generate signed URL parameters
    //             let expiry = SignatureUtil::generate_expiry(12);
    //             // Sign just the encoded URL to avoid path mismatch issues
    //             let signature = services
    //                 .signature_util
    //                 .generate_signature(client_id, expiry, &encoded);

    //             format!(
    //                 "/api/v1/proxy?url={}&schema=movie&sig={}&exp={}&client={}",
    //                 encoded,
    //                 signature,
    //                 expiry,
    //                 urlencoding::encode(client_id)
    //             )
    //         })
    //         .collect();

    //     Ok(lines.join("\n"))
    // }
}
