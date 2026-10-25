// Fake in-process NNTP server (TcpListener): greeting, AUTHINFO, GROUP,
// BODY with dot-stuffed yEnc articles. Drives download_nzb end-to-end.
use downloadcraft_lib::engine::{build_client, Controls};
use downloadcraft_lib::model::*;
use downloadcraft_lib::nntp::crc32;
use downloadcraft_lib::stream::download_url_with_settings;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// Minimal yEnc encoder (single line, escapes critical chars).
fn yenc(data: &[u8], name: &str) -> Vec<u8> {
    let mut out = format!("=ybegin size={} name={name}\r\n", data.len()).into_bytes();
    for &b in data {
        let c = b.wrapping_add(42);
        if matches!(c, 0 | 9 | 10 | 13 | 61) {
            out.push(b'=');
            out.push(c.wrapping_add(64));
        } else {
            out.push(c);
        }
    }
    out.extend_from_slice(
        format!(
            "\r\n=yend size={} pcrc32={:08x}\r\n",
            data.len(),
            crc32(data)
        )
        .as_bytes(),
    );
    out
}

struct Fake {
    cmds: Arc<Mutex<Vec<String>>>,
    peak_body: Arc<std::sync::atomic::AtomicUsize>,
    port: u16,
}

fn fake_nntp(articles: Vec<(String, Vec<u8>)>) -> Fake {
    fake_nntp_delay(articles, 0)
}

fn fake_nntp_delay(articles: Vec<(String, Vec<u8>)>, delay_ms: u64) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let cmds: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let cmds2 = cmds.clone();
    let peak_body = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let peak2 = peak_body.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let articles = articles.clone();
            let cmds = cmds2.clone();
            let peak = peak2.clone();
            std::thread::spawn(move || serve_conn(&mut s, &articles, &cmds, delay_ms, &peak));
        }
    });
    Fake {
        cmds,
        peak_body,
        port,
    }
}

fn read_cmd(s: &mut TcpStream) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match s.read(&mut byte) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {
                if byte[0] == b'\n' {
                    return Some(String::from_utf8_lossy(&buf).trim_end().to_string());
                }
                buf.push(byte[0]);
            }
        }
    }
}

fn serve_conn(
    s: &mut TcpStream,
    articles: &[(String, Vec<u8>)],
    cmds: &Arc<Mutex<Vec<String>>>,
    delay_ms: u64,
    peak_body: &Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::atomic::Ordering::SeqCst;
    static CUR_BODY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let _ = s.write_all(b"200 fake NNTP ready\r\n");
    while let Some(line) = read_cmd(s) {
        cmds.lock().unwrap().push(line.clone());
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("AUTHINFO USER") {
            let _ = s.write_all(b"381 password required\r\n");
        } else if upper.starts_with("AUTHINFO PASS") {
            let _ = s.write_all(b"281 welcome\r\n");
        } else if upper.starts_with("GROUP ") {
            let _ = s.write_all(b"211 3 1 3 alt.binaries.test\r\n");
        } else if upper.starts_with("BODY ") {
            let id = line[5..].trim().trim_matches(|c| c == '<' || c == '>');
            match articles.iter().find(|(k, _)| k == id) {
                Some((_, body)) => {
                    let _ = s.write_all(format!("222 0 <{id}> article retrieved\r\n").as_bytes());
                    let cur = CUR_BODY.fetch_add(1, SeqCst) + 1;
                    peak_body.fetch_max(cur, SeqCst);
                    if delay_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    }
                    CUR_BODY.fetch_sub(1, SeqCst);
                    // Dot-stuff lines starting with '.' per NNTP.
                    for l in body.split(|&b| b == b'\n') {
                        if l.starts_with(b".") {
                            let _ = s.write_all(b".");
                        }
                        let _ = s.write_all(l);
                        let _ = s.write_all(b"\n");
                    }
                    let _ = s.write_all(b".\r\n");
                }
                None => {
                    let _ = s.write_all(b"430 no such article\r\n");
                }
            }
        } else if upper.starts_with("QUIT") {
            let _ = s.write_all(b"205 bye\r\n");
            return;
        } else {
            let _ = s.write_all(b"500 unknown\r\n");
        }
    }
}

/// Hand-crafted Stored (method 0) ZIP: local header + central dir + EOCD.
/// Uses nntp::crc32, no zip writer dep needed test-side.
fn stored_zip(name: &str, data: &[u8]) -> Vec<u8> {
    let crc = crc32(data);
    let mut out = Vec::new();
    let name_b = name.as_bytes();
    out.extend_from_slice(b"PK\x03\x04");
    out.extend_from_slice(&20u16.to_le_bytes()); // version
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&0u16.to_le_bytes()); // method 0
    out.extend_from_slice(&0u16.to_le_bytes()); // time
    out.extend_from_slice(&0u16.to_le_bytes()); // date
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // extra len
    out.extend_from_slice(name_b);
    out.extend_from_slice(data);
    let cd_offset = out.len() as u32;
    out.extend_from_slice(b"PK\x01\x02");
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // extra
    out.extend_from_slice(&0u16.to_le_bytes()); // comment
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
    out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
    out.extend_from_slice(&0u32.to_le_bytes()); // local offset
    out.extend_from_slice(name_b);
    let cd_size = out.len() as u32 - cd_offset;
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("downloadcraft-nntp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

type SegSpec = (&'static str, u64, u32);

/// (subject, [(id, bytes, number)]) — explicit NZB numbers so tests can list
/// segments out of order.
fn nzb_xml(files: &[(&str, Vec<SegSpec>)]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\"?>\n<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n",
    );
    for (subject, segs) in files {
        out.push_str(&format!("<file poster=\"p@x\" date=\"1700000000\" subject=\"{subject}\"><groups><group>alt.binaries.test</group></groups><segments>"));
        for (id, bytes, number) in segs {
            out.push_str(&format!(
                "<segment bytes=\"{bytes}\" number=\"{number}\">{id}</segment>"
            ));
        }
        out.push_str("</segments></file>\n");
    }
    out.push_str("</nzb>");
    out
}

fn settings_for(fake: &Fake) -> Settings {
    Settings {
        nntp_host: "127.0.0.1".into(),
        nntp_port: fake.port,
        nntp_tls: false,
        nntp_user: "alice".into(),
        nntp_pass: "secret".into(),
        ..Default::default()
    }
}

async fn run_nzb(
    dir: &std::path::Path,
    nzb_path: &std::path::Path,
    settings: &Settings,
) -> (DownloadTask, Arc<Controls>) {
    let task = DownloadTask {
        id: "n1".into(),
        url: nzb_path.to_string_lossy().into_owned(),
        filename: "x.nzb".into(),
        dest_dir: dir.to_path_buf(),
        ..Default::default()
    };
    let ctrl = Arc::new(Controls::new(0));
    let done = download_url_with_settings(build_client(""), task, ctrl.clone(), settings)
        .await
        .unwrap();
    (done, ctrl)
}

#[tokio::test]
async fn nntp_downloads_nzb_files_via_fake_server() {
    // Data spans all byte values (yEnc escapes + high bytes included).
    let data1: Vec<u8> = (0u32..600).map(|i| (i % 256) as u8).collect();
    let data2: Vec<u8> = (0u32..300).map(|i| (255 - (i % 256)) as u8).collect();
    let art = |d: &[u8], n: &str| yenc(d, n);
    let fake = fake_nntp(vec![
        ("s1@news".into(), art(&data1[..300], "file1.bin")),
        ("s2@news".into(), art(&data1[300..], "file1.bin")),
        ("s3@news".into(), art(&data2, "file2.bin")),
    ]);
    // nzb_xml emits subjects raw into XML attrs, so quotes arrive pre-escaped.
    let nzb = nzb_xml(&[
        (
            "poster &quot;file1.bin&quot; yEnc (1/2)",
            vec![("s2@news", 320, 2), ("s1@news", 320, 1)],
        ),
        (
            "poster &quot;file2.bin&quot; yEnc (1/1)",
            vec![("s3@news", 320, 1)],
        ),
    ]);
    // Segments listed out of order in file1: NZB number order must win (s1,s2).
    let dir = tmpdir("ok");
    let nzb_path = dir.join("test.nzb");
    std::fs::write(&nzb_path, &nzb).unwrap();
    let settings = settings_for(&fake);
    let (done, ctrl) = run_nzb(&dir, &nzb_path, &settings).await;
    assert_eq!(done.status, TaskStatus::Complete, "{}", done.error);
    // Multi-file NZB: folder named from NZB source stem (test.nzb -> test).
    assert_eq!(done.filename, "test");
    assert_eq!(done.dest_dir, dir);
    let out = dir.join("test");
    assert_eq!(std::fs::read(out.join("file1.bin")).unwrap(), data1);
    assert_eq!(std::fs::read(out.join("file2.bin")).unwrap(), data2);
    assert_eq!(done.downloaded, (data1.len() + data2.len()) as u64);
    assert_eq!(done.total, Some((data1.len() + data2.len()) as u64));
    assert_eq!(
        ctrl.downloaded.load(std::sync::atomic::Ordering::Relaxed),
        (data1.len() + data2.len()) as u64
    );
    // Multi-file NZB: per-file byte parts (on-wire totals 640/320, snapped full).
    assert!(!ctrl
        .parts_are_segments
        .load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(
        ctrl.parts.lock().unwrap().clone(),
        vec![(640, 640), (320, 320)]
    );
    // Sidecars cleaned up on full success.
    assert!(!out.join("file1.bin.nzbstate").exists());
    let cmds = fake.cmds.lock().unwrap().clone();
    assert!(cmds.iter().any(|c| c == "AUTHINFO USER alice"), "{cmds:?}");
    assert!(cmds.iter().any(|c| c == "AUTHINFO PASS secret"), "{cmds:?}");
    assert!(
        cmds.iter().any(|c| c == "GROUP alt.binaries.test"),
        "{cmds:?}"
    );
    assert!(cmds.iter().any(|c| c == "BODY <s1@news>"), "{cmds:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nntp_missing_article_430_errors() {
    let fake = fake_nntp(vec![]);
    let nzb = nzb_xml(&[("poster &quot;gone.bin&quot;", vec![("nope@news", 100, 1)])]);
    let dir = tmpdir("430");
    let nzb_path = dir.join("test.nzb");
    std::fs::write(&nzb_path, &nzb).unwrap();
    let settings = settings_for(&fake);
    let task = DownloadTask {
        id: "n2".into(),
        url: nzb_path.to_string_lossy().into_owned(),
        filename: "x.nzb".into(),
        dest_dir: dir.clone(),
        ..Default::default()
    };
    let r = download_url_with_settings(
        build_client(""),
        task,
        Arc::new(Controls::new(0)),
        &settings,
    )
    .await;
    assert!(r.is_err(), "430 must error, got {r:?}");
    let e = r.unwrap_err();
    assert!(e.contains("430") && e.contains("gone.bin"), "{e}");
    // Damaged file retained (padded) for PAR2 attempt; no par2 set here.
    assert!(dir.join("gone.bin").exists(), "incomplete file retained");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nntp_health_abort_stops_unusable_nzb_early() {
    // 200 segments, none on the server: after the 64-article sample is
    // 100% missing the download must abort ("NZB unusable") instead of
    // fetching all 200 (and without retrying any 430 — one BODY each).
    let fake = fake_nntp(vec![]);
    let segs: Vec<(&'static str, u64, u32)> = (0..200)
        .map(|i| (Box::leak(format!("m{i}@news").into_boxed_str()) as &'static str, 100, i + 1))
        .collect();
    let nzb = nzb_xml(&[(
        "poster &quot;gone.bin&quot;",
        segs.iter().map(|(id, b, n)| (*id, *b, *n)).collect(),
    )]);
    let dir = tmpdir("health430");
    let nzb_path = dir.join("test.nzb");
    std::fs::write(&nzb_path, &nzb).unwrap();
    let mut settings = settings_for(&fake);
    settings.nntp_connections = 1;
    let task = DownloadTask {
        id: "nhealth".into(),
        url: nzb_path.to_string_lossy().into_owned(),
        filename: "x.nzb".into(),
        dest_dir: dir.clone(),
        ..Default::default()
    };
    let r = download_url_with_settings(
        build_client(""),
        task,
        Arc::new(Controls::new(0)),
        &settings,
    )
    .await;
    let e = r.expect_err("unusable NZB must error");
    assert!(e.contains("NZB unusable"), "{e}");
    assert!(e.contains("100%"), "{e}");
    // Early abort: far fewer than 200 BODYs (check fires at 64; a
    // couple may be in flight), and no 430 was retried (still < 200
    // total attempts even counting in-flight).
    let bodies = fake
        .cmds
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.starts_with("BODY "))
        .count();
    assert!(
        (64..70).contains(&bodies),
        "expected abort after ~64 BODYs, got {bodies}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nntp_missing_file_fails_others_continue_resume_skips_done() {
    let data1: Vec<u8> = (0u32..600).map(|i| (i % 256) as u8).collect();
    let data2: Vec<u8> = (0u32..300).map(|i| (255 - (i % 256)) as u8).collect();
    let nzb = nzb_xml(&[
        (
            "poster &quot;file1.bin&quot; yEnc (1/2)",
            vec![("s1@news", 320, 1), ("s2@news", 320, 2)],
        ),
        (
            "poster &quot;file2.bin&quot; yEnc (1/1)",
            vec![("s3@news", 320, 1)],
        ),
    ]);
    let dir = tmpdir("resume430");
    let nzb_path = dir.join("test.nzb");
    std::fs::write(&nzb_path, &nzb).unwrap();
    // Run 1: file2 article absent (430) — file1 must still complete.
    let fake1 = fake_nntp(vec![
        ("s1@news".into(), yenc(&data1[..300], "file1.bin")),
        ("s2@news".into(), yenc(&data1[300..], "file1.bin")),
    ]);
    let settings1 = settings_for(&fake1);
    let task = DownloadTask {
        id: "n4".into(),
        url: nzb_path.to_string_lossy().into_owned(),
        filename: "x.nzb".into(),
        dest_dir: dir.clone(),
        ..Default::default()
    };
    let ctrl = Arc::new(Controls::new(0));
    let r =
        download_url_with_settings(build_client(""), task.clone(), ctrl.clone(), &settings1).await;
    let e = r.unwrap_err();
    assert!(e.contains("file2.bin") && e.contains("430"), "{e}");
    let out1 = dir.join("test");
    assert_eq!(std::fs::read(out1.join("file1.bin")).unwrap(), data1);
    // Failed file retained partial (padded hole candidate for PAR2).
    assert!(out1.join("file2.bin").exists(), "failed file dest retained");
    assert!(
        out1.join("file1.bin.nzbstate").exists(),
        "done file sidecar kept"
    );
    assert!(
        !out1.join("file2.bin.nzbstate").exists(),
        "failed file no sidecar"
    );
    assert!(!ctrl
        .parts_are_segments
        .load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(
        ctrl.parts.lock().unwrap().clone(),
        vec![(640, 640), (0, 320)],
        "failed file part reset, done file full"
    );
    // Run 2: server gains file2; file1 skipped via sidecar (no BODY s1/s2).
    let fake2 = fake_nntp(vec![("s3@news".into(), yenc(&data2, "file2.bin"))]);
    let settings2 = settings_for(&fake2);
    let ctrl2 = Arc::new(Controls::new(0));
    let done = download_url_with_settings(build_client(""), task, ctrl2.clone(), &settings2)
        .await
        .unwrap();
    assert_eq!(done.status, TaskStatus::Complete, "{}", done.error);
    assert_eq!(done.downloaded, 900);
    assert_eq!(std::fs::read(out1.join("file1.bin")).unwrap(), data1);
    assert_eq!(std::fs::read(out1.join("file2.bin")).unwrap(), data2);
    assert_eq!(
        ctrl2.parts.lock().unwrap().clone(),
        vec![(640, 640), (320, 320)]
    );
    let cmds = fake2.cmds.lock().unwrap().clone();
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("s1@news") || c.contains("s2@news")),
        "completed file not refetched: {cmds:?}"
    );
    assert!(cmds.iter().any(|c| c == "BODY <s3@news>"), "{cmds:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// yEnc with `=ypart begin/end` (1-based inclusive): NNTP writer zero-pads
/// holes to `begin-1` before appending, so a missing middle segment leaves
/// an aligned hole PAR2 can rebuild.
fn yenc_part(data: &[u8], name: &str, total: usize, begin: usize) -> Vec<u8> {
    // begin is 1-based offset of data[0].
    let mut out = format!(
        "=ybegin part={} size={total} name={name}\r\n=ypart begin={begin} end={}\r\n",
        1,
        begin + data.len() - 1
    )
    .into_bytes();
    for &b in data {
        let c = b.wrapping_add(42);
        if matches!(c, 0 | 9 | 10 | 13 | 61) {
            out.push(b'=');
            out.push(c.wrapping_add(64));
        } else {
            out.push(c);
        }
    }
    out.extend_from_slice(
        format!(
            "\r\n=yend size={} part=1 pcrc32={:08x}\r\n",
            data.len(),
            crc32(data)
        )
        .as_bytes(),
    );
    out
}

#[tokio::test]
async fn nntp_par2_repairs_missing_segment() {
    use downloadcraft_lib::par2;
    // Scratch dir: source bytes + PAR2 set built there. NZB must NOT be in
    // scratch (create_set scans it); dest separate so NZB/par2 arrive via fake.
    let root = tmpdir("par2e2e");
    let scratch = root.join("scratch");
    let dest = root.join("dest");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    let data: Vec<u8> = (0u32..200).map(|i| ((i * 37 + 11) % 256) as u8).collect();
    std::fs::write(scratch.join("movie.bin"), &data).unwrap();
    let names = par2::test_support::create_set(&scratch, 64, 2).unwrap();
    assert_eq!(names.len(), 2, "{names:?}");
    let (index_name, vol_name) = (names[0].clone(), names[1].clone());
    let index_bytes = std::fs::read(scratch.join(&index_name)).unwrap();
    let vol_bytes = std::fs::read(scratch.join(&vol_name)).unwrap();
    // Segments: movie.bin in 3 chunks 64/64/72, middle omitted (430).
    // yEnc parts carry begin so writer pads hole 64..128 with zeros.
    let chunk = |a: &[u8], b: usize| yenc_part(a, "movie.bin", data.len(), b);
    let fake = fake_nntp(vec![
        ("pm1@news".into(), chunk(&data[0..64], 1)),
        ("pm3@news".into(), chunk(&data[128..200], 129)),
        ("pi1@news".into(), yenc(&index_bytes, &index_name)),
        ("pv1@news".into(), yenc(&vol_bytes, &vol_name)),
    ]);
    // NZB multi-file "movie.nzb" -> out dir dest/movie (source stem).
    let nzb = nzb_xml(&[
        (
            "p &quot;movie.bin&quot;",
            vec![
                ("pm1@news", 320, 1),
                ("pm2@news", 320, 2),
                ("pm3@news", 320, 3),
            ],
        ),
        ("p &quot;set.par2&quot;", vec![("pi1@news", 4096, 1)]),
        (
            &format!("p &quot;{vol_name}&quot;"),
            vec![("pv1@news", 8192, 1)],
        ),
    ]);
    // create_set names index <stem>.par2 with stem=movie, but NZB contract
    // above uses literal "set.par2"; rename index to set.par2 in NZB only
    // if create_set stem differs — stem is movie, so use movie.par2.
    let nzb = nzb.replace("set.par2", "movie.par2");
    let nzb_path = root.join("movie.nzb");
    std::fs::write(&nzb_path, &nzb).unwrap();
    let settings = settings_for(&fake);
    let task = DownloadTask {
        id: "npar2".into(),
        url: nzb_path.to_string_lossy().into_owned(),
        filename: "movie.nzb".into(),
        dest_dir: dest.clone(),
        ..Default::default()
    };
    let ctrl = Arc::new(Controls::new(0));
    let done = download_url_with_settings(build_client(""), task, ctrl.clone(), &settings)
        .await
        .unwrap();
    assert_eq!(done.status, TaskStatus::Complete, "{}", done.error);
    let out = dest.join("movie");
    assert_eq!(std::fs::read(out.join("movie.bin")).unwrap(), data);
    assert!(out.join("movie.par2").exists());
    assert!(out.join(&vol_name).exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn nntp_auto_unpack_extracts_and_deletes_archive() {
    let dir = tmpdir("unpack-ok");
    let payload: &[u8] = b"hello unpack world";
    let zip = stored_zip("hello.txt", payload);
    let fake = fake_nntp(vec![("z1@news".into(), yenc(&zip, "pack.zip"))]);
    let nzb_path = dir.join("test.nzb");
    std::fs::write(
        &nzb_path,
        nzb_xml(&[(
            "p &quot;pack.zip&quot;",
            vec![("z1@news", zip.len() as u64 + 64, 1)],
        )]),
    )
    .unwrap();
    let mut settings = settings_for(&fake);
    settings.nzb_auto_unpack = true;
    let (done, _ctrl) = run_nzb(&dir, &nzb_path, &settings).await;
    assert_eq!(done.status, TaskStatus::Complete, "{}", done.error);
    assert_eq!(std::fs::read(dir.join("hello.txt")).unwrap(), payload);
    assert!(!dir.join("pack.zip").exists(), "archive deleted on success");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nntp_auto_unpack_corrupt_archive_kept_and_errors() {
    let dir = tmpdir("unpack-bad");
    let mut zip = stored_zip("hello.txt", b"hello unpack world");
    let pos = zip
        .windows(18)
        .position(|w| w == b"hello unpack world")
        .unwrap();
    zip[pos] ^= 0xFF; // payload corrupt -> ZIP CRC fails; yEnc crc covers corrupt bytes
    let fake = fake_nntp(vec![("z1@news".into(), yenc(&zip, "pack.zip"))]);
    let nzb_path = dir.join("test.nzb");
    std::fs::write(
        &nzb_path,
        nzb_xml(&[(
            "p &quot;pack.zip&quot;",
            vec![("z1@news", zip.len() as u64 + 64, 1)],
        )]),
    )
    .unwrap();
    let mut settings = settings_for(&fake);
    settings.nzb_auto_unpack = true;
    let task = DownloadTask {
        id: "nbad".into(),
        url: nzb_path.to_string_lossy().into_owned(),
        filename: "x.nzb".into(),
        dest_dir: dir.clone(),
        ..Default::default()
    };
    let r = download_url_with_settings(
        build_client(""),
        task,
        Arc::new(Controls::new(0)),
        &settings,
    )
    .await;
    let err = r.expect_err("corrupt archive must error, not Complete");
    assert!(err.contains("unpack"), "{err}");
    assert_eq!(
        std::fs::read(dir.join("pack.zip")).unwrap(),
        zip,
        "archive kept"
    );
    assert!(!dir.join("hello.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nntp_unconfigured_host_errors() {
    let dir = tmpdir("cfg");
    let nzb_path = dir.join("test.nzb");
    std::fs::write(
        &nzb_path,
        nzb_xml(&[("p &quot;f.bin&quot;", vec![("a@b", 1, 1)])]),
    )
    .unwrap();
    let task = DownloadTask {
        id: "n3".into(),
        url: nzb_path.to_string_lossy().into_owned(),
        filename: "x.nzb".into(),
        dest_dir: dir.clone(),
        ..Default::default()
    };
    let r = download_url_with_settings(
        build_client(""),
        task,
        Arc::new(Controls::new(0)),
        &Settings::default(),
    )
    .await;
    assert!(r.is_err() && r.unwrap_err().contains("not configured"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nntp_parallel_connections_bounded_and_ordered() {
    let data: Vec<u8> = (0..600u32).map(|i| (i % 251) as u8).collect();
    let articles: Vec<(String, Vec<u8>)> = data
        .chunks(100)
        .enumerate()
        .map(|(i, c)| (format!("p{i}@news"), yenc(c, &format!("s{i}"))))
        .collect();
    let fake = fake_nntp_delay(articles, 60);
    let mut settings = settings_for(&fake);
    settings.nntp_connections = 2;
    let dir = tmpdir("par");
    let seg_refs: Vec<(&str, u64, u32)> = vec![
        ("p0@news", 100, 1),
        ("p1@news", 100, 2),
        ("p2@news", 100, 3),
        ("p3@news", 100, 4),
        ("p4@news", 100, 5),
        ("p5@news", 100, 6),
    ];
    let nzb_path = dir.join("multi.nzb");
    std::fs::write(
        &nzb_path,
        nzb_xml(&[("a &quot;multi.bin&quot; (1/1)", seg_refs)]),
    )
    .unwrap();
    let task = DownloadTask {
        id: "npar".into(),
        url: nzb_path.to_string_lossy().into_owned(),
        filename: "multi.nzb".into(),
        dest_dir: dir.clone(),
        ..Default::default()
    };
    let ctrl = Arc::new(Controls::new(0));
    let done = download_url_with_settings(build_client(""), task, ctrl, &settings)
        .await
        .unwrap();
    assert_eq!(done.status, TaskStatus::Complete, "{}", done.error);
    // Single file in NZB: real filename with extension, flat in dest_dir.
    assert_eq!(done.filename, "multi.bin");
    assert_eq!(std::fs::read(dir.join("multi.bin")).unwrap(), data);
    // Exactly 2 server connections opened for whole NZB (shared pool).
    let auths = fake
        .cmds
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.starts_with("AUTHINFO PASS"))
        .count();
    assert_eq!(
        auths,
        2,
        "expected 2 NNTP connections, cmds: {:?}",
        fake.cmds.lock().unwrap()
    );
    let peak = fake.peak_body.load(std::sync::atomic::Ordering::SeqCst);
    assert!(peak >= 2, "BODY fetches never overlapped (peak {peak})");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nntp_files_share_one_connection_pool() {
    let data1: Vec<u8> = (0u32..300).map(|i| (i % 256) as u8).collect();
    let data2: Vec<u8> = (0u32..300).map(|i| (200 - (i % 200)) as u8).collect();
    let fake = fake_nntp_delay(
        vec![
            ("a1@news".into(), yenc(&data1[..100], "f1.bin")),
            ("a2@news".into(), yenc(&data1[100..200], "f1.bin")),
            ("a3@news".into(), yenc(&data1[200..], "f1.bin")),
            ("b1@news".into(), yenc(&data2[..100], "f2.bin")),
            ("b2@news".into(), yenc(&data2[100..200], "f2.bin")),
            ("b3@news".into(), yenc(&data2[200..], "f2.bin")),
        ],
        50,
    );
    let mut settings = settings_for(&fake);
    settings.nntp_connections = 2;
    let dir = tmpdir("sharedpool");
    let nzb = nzb_xml(&[
        (
            "p &quot;f1.bin&quot; (1/1)",
            vec![
                ("a1@news", 320, 1),
                ("a2@news", 320, 2),
                ("a3@news", 320, 3),
            ],
        ),
        (
            "p &quot;f2.bin&quot; (1/1)",
            vec![
                ("b1@news", 320, 1),
                ("b2@news", 320, 2),
                ("b3@news", 320, 3),
            ],
        ),
    ]);
    let nzb_path = dir.join("pack.nzb");
    std::fs::write(&nzb_path, &nzb).unwrap();
    let task = DownloadTask {
        id: "nshared".into(),
        url: nzb_path.to_string_lossy().into_owned(),
        filename: "pack.nzb".into(),
        dest_dir: dir.clone(),
        ..Default::default()
    };
    let ctrl = Arc::new(Controls::new(0));
    let done = download_url_with_settings(build_client(""), task, ctrl.clone(), &settings)
        .await
        .unwrap();
    assert_eq!(done.status, TaskStatus::Complete, "{}", done.error);
    let out = dir.join("pack");
    assert_eq!(std::fs::read(out.join("f1.bin")).unwrap(), data1);
    assert_eq!(std::fs::read(out.join("f2.bin")).unwrap(), data2);
    let cmds = fake.cmds.lock().unwrap().clone();
    let auths = cmds
        .iter()
        .filter(|c| c.starts_with("AUTHINFO PASS"))
        .count();
    assert_eq!(
        auths, 2,
        "pool must span files, not per-file pools: {cmds:?}"
    );
    assert_eq!(
        ctrl.parts.lock().unwrap().clone(),
        vec![(960, 960), (960, 960)]
    );
    let _ = std::fs::remove_dir_all(&dir);
}
