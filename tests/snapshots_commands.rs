//! Snapshot tests that replay captured XBDM traffic through `MockTransport`,
//! execute a typed command, and snapshot the parsed output via `insta`.
//!
//! Captures live under `tests/fixtures/*.capture`. Running `cargo insta
//! review` after a change rolls the snapshots forward.

use std::path::PathBuf;

use xeedee::Client;
use xeedee::commands::AltAddr;
use xeedee::commands::DbgName;
use xeedee::commands::DirList;
use xeedee::commands::DriveFreeSpace;
use xeedee::commands::DriveList;
use xeedee::commands::FileUploadKind;
use xeedee::commands::GetFileAttributes;
use xeedee::commands::GetFileRange;
use xeedee::commands::IsStopped;
use xeedee::commands::Modules;
use xeedee::commands::SetMem;
use xeedee::commands::SysTime;
use xeedee::commands::ThreadId;
use xeedee::commands::Threads;
use xeedee::commands::XbeInfo;
use xeedee::transport::CaptureLog;
use xeedee::transport::MockTransport;

fn fixture(name: &str) -> CaptureLog {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures");
    path.push(format!("{name}.capture"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()));
    CaptureLog::from_text(&text).expect("parsing capture fixture")
}

fn run_command<C>(fixture_name: &str, command: C) -> C::Output
where
    C: xeedee::Command,
{
    let log = fixture(fixture_name);
    let mock = MockTransport::from_log(log);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let mut client = Client::new(mock).read_banner().await.unwrap();
        client.run(command).await.unwrap()
    })
}

#[test]
fn dbgname_get_parses_canned_response() {
    let name = run_command("dbgname_get", DbgName::Get);
    insta::assert_debug_snapshot!(name, @r###""deanxbox""###);
}

#[test]
fn systime_parses_filetime() {
    let result = run_command("systime", SysTime);
    insta::assert_debug_snapshot!(result, @r###"
    SysTimeResult {
        file_time: FileTime(
            133476389166481920,
        ),
    }
    "###);
}

#[test]
fn drivelist_parses_multiline() {
    let drives = run_command("drivelist", DriveList);
    insta::assert_debug_snapshot!(drives, @r###"
    [
        "D",
        "E",
        "Z",
    ]
    "###);
}

#[test]
fn drivelist_360_multi_segment_chunk() {
    let drives = run_command("drivelist_360", DriveList);
    insta::assert_debug_snapshot!(drives, @r###"
    [
        "SysCache0",
        "SysCache1",
        "SysCache2",
        "E",
        "DEVKIT",
        "HDD",
        "MUINT",
        "INTUSB",
    ]
    "###);
}

#[test]
fn drivefreespace_parses_hi_lo_pairs() {
    let space = run_command(
        "drivefreespace",
        DriveFreeSpace {
            drive: "DEVKIT:\\".to_owned(),
        },
    );
    insta::assert_debug_snapshot!(space, @r###"
    DriveSpace {
        free_to_caller_bytes: 216171921408,
        total_bytes: 233545236480,
        total_free_bytes: 216171921408,
    }
    "###);
}

#[test]
fn dirlist_parses_entries_with_mixed_types() {
    let entries = run_command(
        "dirlist_devkit",
        DirList {
            path: "DEVKIT:\\".to_owned(),
        },
    );
    insta::assert_debug_snapshot!(entries, @r###"
    [
        DirEntry {
            name: "dmext",
            size: 0,
            create_time: FileTime(
                130407778600000000,
            ),
            change_time: FileTime(
                130407778600000000,
            ),
            is_directory: true,
        },
        DirEntry {
            name: "music1.wma",
            size: 243079,
            create_time: FileTime(
                130228794180000000,
            ),
            change_time: FileTime(
                130228794180000000,
            ),
            is_directory: false,
        },
    ]
    "###);
}

#[test]
fn getfileattributes_parses_single_entry() {
    let attrs = run_command(
        "getfileattributes",
        GetFileAttributes {
            path: "DEVKIT:\\music1.wma".to_owned(),
        },
    );
    insta::assert_debug_snapshot!(attrs, @r###"
    FileAttributes {
        size: 243079,
        create_time: FileTime(
            130228794180000000,
        ),
        change_time: FileTime(
            130228794180000000,
        ),
        is_directory: false,
    }
    "###);
}

#[test]
fn getfile_streams_prefixed_payload() {
    let log = fixture("getfile_small");
    let mock = MockTransport::from_log(log).with_lax_writes();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (bytes, total) = runtime.block_on(async move {
        let mut client = Client::new(mock).read_banner().await.unwrap();
        let download = client
            .get_file("E:\\greet.txt", GetFileRange::WholeFile)
            .await
            .unwrap();
        let total = download.total();
        let bytes = download.into_vec().await.unwrap();
        (bytes, total)
    });
    assert_eq!(total, 14);
    assert_eq!(bytes, b"hello, xbdm!\r\n");
}

#[test]
fn altaddr_parses_ipv4() {
    let addr = run_command("altaddr", AltAddr);
    insta::assert_debug_snapshot!(addr, @"192.168.1.25");
}

#[test]
fn modules_parse_flags_and_sizes() {
    let mods = run_command("modules_small", Modules);
    insta::assert_debug_snapshot!(mods, @r###"
    [
        ModuleInfo {
            name: "xboxkrnl.exe",
            base: 2147745792,
            size: 2359296,
            checksum: 1862603,
            timestamp: 1385584266,
            pdata: 2147925504,
            psize: 28592,
            thread: 0,
            osize: 2359296,
            is_dll: false,
            is_tls: false,
            is_xbe: false,
        },
        ModuleInfo {
            name: "xbdm.xex",
            base: 2448424960,
            size: 716800,
            checksum: 741891,
            timestamp: 1378429206,
            pdata: 2448488448,
            psize: 11904,
            thread: 0,
            osize: 802816,
            is_dll: true,
            is_tls: false,
            is_xbe: false,
        },
    ]
    "###);
}

#[test]
fn threads_parse_signed_decimal_as_unsigned() {
    let ids = run_command("threads", Threads);
    insta::assert_debug_snapshot!(ids, @r###"
    [
        ThreadId(
            4211081304,
        ),
        ThreadId(
            4211081296,
        ),
    ]
    "###);
}

#[test]
fn xbeinfo_parses_running_title() {
    let info = run_command("xbeinfo_running", XbeInfo::Running);
    insta::assert_debug_snapshot!(info, @r###"
    XbeInfoResult {
        timestamp: 0,
        checksum: 0,
        name: "\\Device\\Flash\\xshell.xex",
    }
    "###);
}

#[test]
fn setmem_parses_written_count() {
    let result = run_command(
        "setmem",
        SetMem {
            address: 0xFEEF_0000,
            data: vec![0xDE, 0xAD, 0xBE, 0xEF],
        },
    );
    insta::assert_debug_snapshot!(result, @r###"
    BytesWritten {
        requested: 4,
        written: 4,
    }
    "###);
}

#[test]
fn sendfile_streams_upload_and_finalizes() {
    let log = fixture("sendfile_small");
    let mock = MockTransport::from_log(log);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let mut client = Client::new(mock).read_banner().await.unwrap();
        let upload = client
            .send_file(r"E:\hi.txt", FileUploadKind::Create { size: 5 })
            .await
            .unwrap();
        upload.send_all(b"hello").await.unwrap();
    });
}

#[test]
fn isstopped_maps_408_to_running() {
    let state = run_command(
        "isstopped_running",
        IsStopped {
            thread: ThreadId(0xfb000018),
        },
    );
    insta::assert_debug_snapshot!(state, @"Running");
}

#[test]
fn error_response_is_typed() {
    let log = fixture("error_unknown");
    let mock = MockTransport::from_log(log).with_lax_writes();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let err = runtime.block_on(async move {
        let mut client = Client::new(mock).read_banner().await.unwrap();
        client.send_raw("notacommand").await.unwrap_err()
    });
    let kind = err.current_context();
    insta::assert_debug_snapshot!(kind, @r###"
    Remote {
        code: UnknownCommand,
        message: "unknown command",
    }
    "###);
}
