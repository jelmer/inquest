//! Tests for TestCommand functionality

use inquest::testcommand::TestCommand;
use std::io::Write;
use tempfile::TempDir;

#[test]
fn test_list_tests_parses_subunit_enumeration() {
    // Create a temporary directory for our test
    let temp = TempDir::new().unwrap();
    let base_path = temp.path();

    // Create a test command script that outputs subunit v2 enumeration events
    // This is actual subunit v2 binary data with two enumeration events
    let subunit_data: &[u8] = &[
        0xb3, 0x29, 0x01, 0x1e, 0x15, 0x74, 0x65, 0x73, 0x74, 0x2e, 0x65, 0x78, 0x61, 0x6d, 0x70,
        0x6c, 0x65, 0x2e, 0x74, 0x65, 0x73, 0x74, 0x5f, 0x6f, 0x6e, 0x65, 0x63, 0xe2, 0xa6, 0x82,
        0xb3, 0x29, 0x01, 0x1e, 0x15, 0x74, 0x65, 0x73, 0x74, 0x2e, 0x65, 0x78, 0x61, 0x6d, 0x70,
        0x6c, 0x65, 0x2e, 0x74, 0x65, 0x73, 0x74, 0x5f, 0x74, 0x77, 0x6f, 0x08, 0x44, 0xaa, 0x15,
    ];

    // Create a shell script that outputs this data
    let script_path = base_path.join("test_script.sh");
    let mut script = std::fs::File::create(&script_path).unwrap();
    writeln!(script, "#!/bin/bash").unwrap();
    write!(script, "printf '").unwrap();
    for byte in subunit_data {
        write!(script, "\\x{:02x}", byte).unwrap();
    }
    writeln!(script, "'").unwrap();
    drop(script);

    // Make script executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
    }

    // Create .testr.conf pointing to our script
    let config_path = base_path.join(".testr.conf");
    let mut config = std::fs::File::create(&config_path).unwrap();
    writeln!(
        config,
        "[DEFAULT]\ntest_command=bash test_script.sh\ntest_list_option=--list"
    )
    .unwrap();

    // Create TestCommand and call list_tests
    let test_command = TestCommand::from_directory(base_path).unwrap();
    let test_ids = test_command.list_tests().unwrap();

    // Verify we got both test IDs
    assert_eq!(test_ids.len(), 2, "Should parse 2 enumeration events");
    assert_eq!(test_ids[0].to_string(), "test.example.test_one");
    assert_eq!(test_ids[1].to_string(), "test.example.test_two");
}

#[test]
fn test_list_tests_forwards_stderr_live() {
    // Verify that list_tests_with_stderr forwards child-process stderr output
    // to the provided sink as it is produced. This is what surfaces build
    // output (e.g. `cargo` compilation) to the user before tests start.
    let temp = TempDir::new().unwrap();
    let base_path = temp.path();

    // Subunit v2 bytes for a single enumeration event on test.example.test_one,
    // so the listing call still succeeds.
    let subunit_data: &[u8] = &[
        0xb3, 0x29, 0x01, 0x1e, 0x15, 0x74, 0x65, 0x73, 0x74, 0x2e, 0x65, 0x78, 0x61, 0x6d, 0x70,
        0x6c, 0x65, 0x2e, 0x74, 0x65, 0x73, 0x74, 0x5f, 0x6f, 0x6e, 0x65, 0x63, 0xe2, 0xa6, 0x82,
    ];

    // Script writes to stderr, sleeps briefly, then emits subunit on stdout.
    let script_path = base_path.join("test_script.sh");
    let mut script = std::fs::File::create(&script_path).unwrap();
    writeln!(script, "#!/bin/bash").unwrap();
    writeln!(script, "echo 'BUILD LINE 1' >&2").unwrap();
    writeln!(script, "echo 'BUILD LINE 2' >&2").unwrap();
    write!(script, "printf '").unwrap();
    for byte in subunit_data {
        write!(script, "\\x{:02x}", byte).unwrap();
    }
    writeln!(script, "'").unwrap();
    drop(script);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
    }

    let config_path = base_path.join(".testr.conf");
    let mut config = std::fs::File::create(&config_path).unwrap();
    writeln!(
        config,
        "[DEFAULT]\ntest_command=bash test_script.sh\ntest_list_option=--list"
    )
    .unwrap();

    let test_command = TestCommand::from_directory(base_path).unwrap();
    let mut stderr_sink: Vec<u8> = Vec::new();
    let test_ids = test_command
        .list_tests_with_stderr(&mut stderr_sink)
        .unwrap();

    assert_eq!(test_ids.len(), 1);
    let forwarded = String::from_utf8(stderr_sink).unwrap();
    assert_eq!(forwarded, "BUILD LINE 1\nBUILD LINE 2\n");
}

#[test]
fn test_list_tests_reports_stderr_on_failure() {
    // When the listing command fails, its stderr should still be included in
    // the error message (not swallowed by the live-forwarding tee).
    let temp = TempDir::new().unwrap();
    let base_path = temp.path();

    let script_path = base_path.join("test_script.sh");
    let mut script = std::fs::File::create(&script_path).unwrap();
    writeln!(script, "#!/bin/bash").unwrap();
    writeln!(script, "echo 'compile error: boom' >&2").unwrap();
    writeln!(script, "exit 1").unwrap();
    drop(script);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
    }

    let config_path = base_path.join(".testr.conf");
    let mut config = std::fs::File::create(&config_path).unwrap();
    writeln!(
        config,
        "[DEFAULT]\ntest_command=bash test_script.sh\ntest_list_option=--list"
    )
    .unwrap();

    let test_command = TestCommand::from_directory(base_path).unwrap();
    let mut stderr_sink: Vec<u8> = Vec::new();
    let err = test_command
        .list_tests_with_stderr(&mut stderr_sink)
        .unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("compile error: boom"),
        "error should include captured stderr, got: {msg}"
    );
    // And it should also have been forwarded live.
    assert_eq!(
        String::from_utf8(stderr_sink).unwrap(),
        "compile error: boom\n"
    );
}
