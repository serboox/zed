use anyhow::{Context as _, Result};
use smol::io::{AsyncReadExt, AsyncWriteExt};
use smol::net::TcpListener;

const REDIRECT_LANDING_PAGE: &str =
    "<html><body><p>You can close this tab and return to Zed.</p></body></html>";

/// Binds an ephemeral loopback TCP port, so `redirect_uri` can be built
/// before the browser is opened -- this is exactly the "loopback HTTP
/// listener" approach the work plan flags as needing its own design: no
/// prior art existed in this codebase for capturing an OAuth 2.0
/// authorization-code redirect, and a loopback listener is the standard
/// native-app pattern (RFC 8252) that avoids needing a public redirect
/// endpoint or a paste-the-code-manually fallback.
pub async fn bind_loopback_port() -> Result<(TcpListener, u16)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("could not bind a local port for the OAuth redirect")?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

/// Accepts exactly one connection on `listener` (the browser's redirect
/// request), reads its request line, responds with a small landing page so
/// the browser tab doesn't hang, and returns the raw request text for the
/// caller to parse with `api_client::parse_authorization_redirect`.
pub async fn accept_one_redirect(listener: TcpListener) -> Result<String> {
    let (mut stream, _) = listener
        .accept()
        .await
        .context("failed to accept the OAuth redirect connection")?;
    let mut buffer = vec![0u8; 8192];
    let bytes_read = stream
        .read(&mut buffer)
        .await
        .context("failed to read the OAuth redirect request")?;
    let request_text = String::from_utf8_lossy(&buffer[..bytes_read]).into_owned();

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        REDIRECT_LANDING_PAGE.len(),
        REDIRECT_LANDING_PAGE
    );
    // Writing the landing page is best-effort: the browser already has the
    // request outcome from the redirect itself, so a failed write here must
    // never fail the whole authorization flow.
    if let Err(error) = stream.write_all(response.as_bytes()).await {
        log::warn!("failed to write the OAuth redirect landing page: {error}");
    }
    Ok(request_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A plain `#[test]` + `smol::block_on`, not `#[gpui::test]` -- gpui's
    // deterministic test scheduler forbids the real OS-level blocking a live
    // TCP accept/connect requires ("Parking forbidden"), so this needs
    // smol's real reactor instead.
    #[test]
    fn a_real_loopback_connection_is_captured_and_its_request_text_returned() {
        smol::block_on(async {
            let (listener, port) = bind_loopback_port().await.unwrap();
            assert!(port > 0);

            let accept_task = smol::spawn(accept_one_redirect(listener));

            let mut client = smol::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            use smol::io::AsyncWriteExt as _;
            client
                .write_all(
                    b"GET /callback?code=abc123&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
                )
                .await
                .unwrap();

            let request_text = accept_task.await.unwrap();
            assert!(request_text.contains("code=abc123"));
            assert!(request_text.contains("state=xyz"));

            let code = api_client::parse_authorization_redirect(&request_text, "xyz").unwrap();
            assert_eq!(code, "abc123");
        });
    }
}
