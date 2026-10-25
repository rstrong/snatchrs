use snatchrs::{PackageIndex, normalize_name};
use url::Url;

#[test]
fn parses_packages_and_serials_from_saved_index() {
    let index = PackageIndex::from_reader(
        include_bytes!("fixtures/index.json").as_slice(),
        Url::parse("https://example.org/mirror/simple").unwrap(),
    )
    .unwrap();
    assert_eq!(index.api_version, "1.4");
    assert_eq!(index.last_serial, Some(12345));
    let packages: Vec<_> = (&index).into_iter().collect();
    assert_eq!(packages.len(), 3);
    assert_eq!(packages[0].name, "Friendly_Bard");
    assert_eq!(packages[0].normalized_name, "friendly-bard");
    assert_eq!(
        packages[0].index_url.as_str(),
        "https://example.org/mirror/simple/friendly-bard/"
    );
    assert_eq!(packages[0].last_serial, Some(123));
    assert_eq!(packages[1].last_serial, None);
}

#[test]
fn normalizes_equivalent_names() {
    for name in [
        "friendly-bard",
        "Friendly_Bard",
        "FRIENDLY.BARD",
        "FrIeNdLy-._.-bArD",
    ] {
        assert_eq!(normalize_name(name).unwrap(), "friendly-bard");
    }
}

#[test]
fn rejects_invalid_package_names() {
    for name in ["", "-pkg", "pkg_", "../pkg", "pkg/name", "café", "pkg\n"] {
        assert!(normalize_name(name).is_err(), "accepted {name:?}");
    }
}

#[test]
fn permits_empty_indexes_and_optional_serials() {
    let index = parse(r#"{"meta":{"api-version":"1.0"},"projects":[]}"#).unwrap();
    assert!(index.packages.is_empty());
    assert_eq!(index.last_serial, None);
}

#[test]
fn rejects_malformed_or_unsupported_indexes() {
    for input in [
        "<html></html>",
        r#"{"meta":{"api-version":"2.0"},"projects":[]}"#,
        r#"{"meta":{"api-version":"1.0"}}"#,
        r#"{"meta":{"api-version":"1.0"},"projects":[{"name":"../bad"}]}"#,
    ] {
        assert!(parse(input).is_err());
    }
}

#[test]
fn requests_json_and_handles_http_errors() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    for (status, body, success) in [
        ("200 OK", include_str!("fixtures/index.json"), true),
        ("503 Service Unavailable", "unavailable", false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!(
            "http://{}/simple/",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 1024];
                let size = stream.read(&mut buffer).unwrap();
                assert!(size > 0, "connection closed before request headers");
                request.extend_from_slice(&buffer[..size]);
                if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    break;
                }
            }
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/vnd.pypi.simple.v1+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            String::from_utf8(request).unwrap()
        });
        let result = snatchrs::IndexClient::new().unwrap().fetch(url);
        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /simple/ http/1.1\r\n"));
        assert!(request.contains("accept: application/vnd.pypi.simple.v1+json\r\n"));
        assert_eq!(result.is_ok(), success);
    }
}

fn parse(input: &str) -> anyhow::Result<PackageIndex> {
    PackageIndex::from_reader(
        input.as_bytes(),
        Url::parse("https://pypi.org/simple/").unwrap(),
    )
}

#[test]
fn progress_tracks_bytes_and_each_validated_package() {
    use snatchrs::IndexProgress;
    let fixture = include_bytes!("fixtures/index.json");
    let mut bytes_read = 0;
    let mut packages = Vec::new();
    PackageIndex::from_reader_with_progress(
        fixture.as_slice(),
        Url::parse("https://pypi.org/simple/").unwrap(),
        |event| match event {
            IndexProgress::Reading { bytes } => {
                assert!(bytes >= bytes_read);
                bytes_read = bytes;
            }
            IndexProgress::Packages {
                processed,
                total,
                name,
            } => {
                assert_eq!(total, 3);
                packages.push((processed, name.to_owned()));
            }
        },
    )
    .unwrap();
    assert_eq!(bytes_read, fixture.len() as u64);
    assert_eq!(
        packages,
        vec![
            (0, String::new()),
            (1, "Friendly_Bard".into()),
            (2, "requests".into()),
            (3, "zope.interface".into()),
        ]
    );
}
