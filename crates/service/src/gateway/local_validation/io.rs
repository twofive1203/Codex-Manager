use tiny_http::Request;

fn request_header_value<'a>(request: &'a Request, header_name: &str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(header_name))
        .map(|header| header.value.as_str().trim())
        .filter(|value| !value.is_empty())
}

fn normalize_forwarded_for_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'');
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("unknown") {
        return None;
    }
    let bracket_trimmed = trimmed
        .strip_prefix('[')
        .and_then(|value| value.split(']').next())
        .unwrap_or(trimmed);
    let value = bracket_trimmed.trim();
    if value.is_empty() {
        return None;
    }
    Some(value.to_string())
}

fn parse_forwarded_header_client_ip(raw: &str) -> Option<String> {
    for item in raw.split(',') {
        for pair in item.split(';') {
            let (name, value) = pair.split_once('=')?;
            if !name.trim().eq_ignore_ascii_case("for") {
                continue;
            }
            if let Some(client_ip) = normalize_forwarded_for_token(value) {
                return Some(client_ip);
            }
        }
    }
    None
}

/// 函数 `extract_request_client_ip`
///
/// 作者: gaohongshun
///
/// 时间: 2026-04-27
///
/// # 参数
/// - request: 参数 request
///
/// # 返回
/// 返回请求来源 IP
pub(crate) fn extract_request_client_ip(request: &Request) -> Option<String> {
    if let Some(value) = request_header_value(request, "X-Forwarded-For") {
        if let Some(first) = value.split(',').next() {
            if let Some(client_ip) = normalize_forwarded_for_token(first) {
                return Some(client_ip);
            }
        }
    }

    if let Some(value) = request_header_value(request, "Forwarded") {
        if let Some(client_ip) = parse_forwarded_header_client_ip(value) {
            return Some(client_ip);
        }
    }

    request.remote_addr().map(|addr| addr.ip().to_string())
}

/// 函数 `read_request_body`
///
/// 作者: gaohongshun
///
/// 时间: 2026-04-02
///
/// # 参数
/// - super: 参数 super
///
/// # 返回
/// 返回函数执行结果
pub(super) fn read_request_body(
    request: &mut Request,
) -> Result<Vec<u8>, super::LocalValidationError> {
    // 中文注释：先把请求体读完再进入鉴权判断，避免客户端写流还在进行时被提前断开。
    let mut body = Vec::new();
    let max_body_bytes = crate::gateway::front_proxy_max_body_bytes();
    let reader = request.as_reader();
    let mut chunk = [0_u8; 8192];

    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };
        body.extend_from_slice(&chunk[..read]);
        if max_body_bytes > 0 && body.len() > max_body_bytes {
            return Err(super::LocalValidationError::new(
                413,
                crate::gateway::bilingual_error(
                    "请求体过大",
                    format!("request body too large: content-length>{max_body_bytes}"),
                ),
            ));
        }
    }

    Ok(body)
}

/// 函数 `extract_platform_key_or_error`
///
/// 作者: gaohongshun
///
/// 时间: 2026-04-02
///
/// # 参数
/// - super: 参数 super
///
/// # 返回
/// 返回函数执行结果
pub(super) fn extract_platform_key_or_error(
    request: &Request,
    incoming_headers: &super::super::IncomingHeaderSnapshot,
    debug: bool,
) -> Result<String, super::LocalValidationError> {
    if let Some(platform_key) = incoming_headers.platform_key() {
        return Ok(platform_key.to_string());
    }

    if debug {
        let remote = request
            .remote_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "<none>".to_string());
        let auth_scheme = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
            .and_then(|h| h.value.as_str().split_whitespace().next())
            .unwrap_or("<none>");
        let header_names = request
            .headers()
            .iter()
            .map(|h| h.field.as_str().as_str())
            .collect::<Vec<_>>()
            .join(",");
        log::warn!(
            "event=gateway_auth_missing path={} status=401 remote={} has_auth={} auth_scheme={} has_x_api_key={} headers=[{}]",
            request.url(),
            remote,
            incoming_headers.has_authorization(),
            auth_scheme,
            incoming_headers.has_x_api_key(),
            header_names,
        );
    }

    Err(super::LocalValidationError::new(
        401,
        crate::gateway::bilingual_error("缺少 API Key", "missing api key"),
    ))
}
