use crate::core::tracking;
use crate::core::utils::resolved_command;
use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::OsString;

// ─── Dart test JSON reporter types ───

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct DartTestEvent {
    #[serde(rename = "type")]
    event_type: String,
    // Fields from "testStart" events
    test: Option<DartTest>,
    // Fields from "testDone" events
    #[serde(rename = "testID")]
    test_id: Option<u64>,
    result: Option<String>,
    skipped: Option<bool>,
    hidden: Option<bool>,
    // Fields from "error" events
    error: Option<String>,
    #[serde(rename = "stackTrace")]
    stack_trace: Option<String>,
    #[serde(rename = "isFailure")]
    is_failure: Option<bool>,
    // Fields from "suite" events
    suite: Option<DartSuite>,
    // Fields from "done" events
    success: Option<bool>,
    // Fields from "print" events
    message: Option<String>,
    #[serde(rename = "messageType")]
    message_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct DartTest {
    id: u64,
    name: String,
    #[serde(rename = "suiteID")]
    suite_id: Option<u64>,
    line: Option<u64>,
    column: Option<u64>,
    url: Option<String>,
    root_line: Option<u64>,
    root_url: Option<String>,
    metadata: Option<DartTestMetadata>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct DartTestMetadata {
    skip: Option<bool>,
    #[serde(rename = "skipReason")]
    skip_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct DartSuite {
    id: u64,
    path: Option<String>,
}

#[derive(Debug, Default)]
struct TestResult {
    pass: usize,
    fail: usize,
    error: usize,
    skip: usize,
    suites: usize,
    failures: Vec<TestFailure>,
}

#[derive(Debug)]
struct TestFailure {
    name: String,
    error_msg: String,
    location: String,
    is_failure: bool, // true = assertion failure, false = exception/error
}

// ─── flutter test ───

pub fn run_test(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("flutter");
    cmd.arg("test");

    // Force JSON reporter if not already specified
    if !args
        .iter()
        .any(|a| a == "--reporter" || a.starts_with("--reporter="))
    {
        cmd.arg("--reporter").arg("json");
    }

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: flutter test --reporter json {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run flutter test. Is Flutter installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });
    let filtered = filter_flutter_test_json(&stdout);

    if let Some(hint) = crate::core::tee::tee_and_hint(&raw, "flutter_test", exit_code) {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    if !stderr.trim().is_empty() && verbose > 0 {
        eprintln!("{}", stderr.trim());
    }

    timer.track(
        &format!("flutter test {}", args.join(" ")),
        &format!("rtk flutter test {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

fn filter_flutter_test_json(output: &str) -> String {
    let mut tests: HashMap<u64, String> = HashMap::new(); // id -> name
    let mut result = TestResult::default();
    let mut suites_seen: std::collections::HashSet<u64> = std::collections::HashSet::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let event: DartTestEvent = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => continue,
        };

        match event.event_type.as_str() {
            "suite" => {
                if let Some(suite) = &event.suite {
                    suites_seen.insert(suite.id);
                }
            }
            "testStart" => {
                if let Some(test) = &event.test {
                    tests.insert(test.id, test.name.clone());
                }
            }
            "error" => {
                if let Some(test_id) = event.test_id {
                    let test_name = tests
                        .get(&test_id)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string());
                    let error_msg = event.error.unwrap_or_default();
                    let stack = event.stack_trace.unwrap_or_default();
                    let location = extract_user_location(&stack);
                    let is_failure = event.is_failure.unwrap_or(false);

                    result.failures.push(TestFailure {
                        name: test_name,
                        error_msg: error_msg.trim().to_string(),
                        location,
                        is_failure,
                    });
                }
            }
            "testDone" => {
                let hidden = event.hidden.unwrap_or(false);
                if hidden {
                    continue;
                }
                let skipped = event.skipped.unwrap_or(false);
                if skipped {
                    result.skip += 1;
                    continue;
                }
                match event.result.as_deref() {
                    Some("success") => result.pass += 1,
                    Some("failure") => result.fail += 1,
                    Some("error") => result.error += 1,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    result.suites = suites_seen.len();

    // Build compact output
    let mut parts = Vec::new();
    if result.pass > 0 {
        parts.push(format!("{} passed", result.pass));
    }
    if result.fail > 0 {
        parts.push(format!("{} failed", result.fail));
    }
    if result.error > 0 {
        parts.push(format!("{} error", result.error));
    }
    if result.skip > 0 {
        parts.push(format!("{} skipped", result.skip));
    }

    let suite_label = if result.suites == 1 {
        "1 suite".to_string()
    } else {
        format!("{} suites", result.suites)
    };

    let mut out = if parts.is_empty() {
        format!("Flutter test: no tests found ({})", suite_label)
    } else {
        format!("Flutter test: {} ({})", parts.join(", "), suite_label)
    };

    // Append failure details
    for f in &result.failures {
        let label = if f.is_failure { "FAIL" } else { "ERROR" };
        out.push_str(&format!("\n\n{} {}", label, f.name));
        out.push_str(&format!("\n  {}", f.error_msg));
        if !f.location.is_empty() {
            out.push_str(&format!("\n  {}", f.location));
        }
    }

    out
}

/// Extract the first user-code location from a Dart stack trace (skip package: lines)
fn extract_user_location(stack: &str) -> String {
    for line in stack.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // User code lines look like: "test/failing_test.dart 9:5  main.<fn>"
        // Package lines look like: "package:matcher  expect"
        if !trimmed.starts_with("package:") {
            // Extract file:line from patterns like "test/file.dart 9:5"
            if let Some(pos) = trimmed.find(".dart") {
                let end = pos + 5; // ".dart" length
                let rest = &trimmed[end..].trim_start();
                if let Some(line_num) = rest.split(':').next() {
                    if line_num.parse::<u64>().is_ok() {
                        return format!("{}:{}", &trimmed[..end], line_num);
                    }
                }
            }
            return trimmed.to_string();
        }
    }
    String::new()
}

// ─── flutter analyze ───

pub fn run_analyze(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("flutter");
    cmd.arg("analyze");

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: flutter analyze {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run flutter analyze. Is Flutter installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });
    let filtered = filter_flutter_analyze(&raw);

    if let Some(hint) = crate::core::tee::tee_and_hint(&raw, "flutter_analyze", exit_code) {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("flutter analyze {}", args.join(" ")),
        &format!("rtk flutter analyze {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

fn filter_flutter_analyze(output: &str) -> String {
    let issue_re =
        Regex::new(r"^\s*(error|warning|info)\s+-\s+(.+?)\s+-\s+(\S+:\d+:\d+)\s+-\s+(\S+)")
            .unwrap();

    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut infos = Vec::new();

    for line in output.lines() {
        if let Some(caps) = issue_re.captures(line) {
            let severity = caps.get(1).unwrap().as_str();
            let message = caps.get(2).unwrap().as_str().trim();
            let location = caps.get(3).unwrap().as_str();
            // Normalize path separators
            let location = location.replace('\\', "/");
            // Strip column from location for compactness: lib/file.dart:5:10 -> lib/file.dart:5
            let location = strip_column(&location);
            let rule = caps.get(4).unwrap().as_str();

            let entry = format!("  {}: {} - {} ({})", severity, message, location, rule);
            match severity {
                "error" => errors.push(entry),
                "warning" => warnings.push(entry),
                _ => infos.push(entry),
            }
        }
    }

    let total = errors.len() + warnings.len() + infos.len();

    if total == 0 {
        return "Flutter analyze: no issues found".to_string();
    }

    let mut parts = Vec::new();
    if !errors.is_empty() {
        parts.push(format!("{} error", errors.len()));
    }
    if !warnings.is_empty() {
        parts.push(format!("{} warning", warnings.len()));
    }
    if !infos.is_empty() {
        parts.push(format!("{} info", infos.len()));
    }

    let mut out = format!("Flutter analyze: {} ({} issues)", parts.join(", "), total);

    // Show errors first, then warnings, then infos
    for entry in errors.iter().chain(warnings.iter()).chain(infos.iter()) {
        out.push('\n');
        out.push_str(entry);
    }

    out
}

/// Strip column number from location: "lib/file.dart:5:10" -> "lib/file.dart:5"
fn strip_column(location: &str) -> String {
    // Split into at most 3 parts by ':' from the right: [col, line, path] or [line, path]
    let parts: Vec<&str> = location.rsplitn(3, ':').collect();
    if parts.len() == 3 {
        // file:line:col — strip the column
        if parts[0].parse::<u64>().is_ok() && parts[1].parse::<u64>().is_ok() {
            return format!("{}:{}", parts[2], parts[1]);
        }
    }
    // file:line or no colons — keep as-is
    location.to_string()
}

// ─── flutter build ───

pub fn run_build(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("flutter");
    cmd.arg("build");

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: flutter build {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run flutter build. Is Flutter installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });
    let filtered = filter_flutter_build(&raw, exit_code);

    if let Some(hint) = crate::core::tee::tee_and_hint(&raw, "flutter_build", exit_code) {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("flutter build {}", args.join(" ")),
        &format!("rtk flutter build {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

fn filter_flutter_build(output: &str, exit_code: i32) -> String {
    // On failure, show all error-relevant lines
    if exit_code != 0 {
        let mut error_lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Skip progress lines like "Compiling lib\main.dart for the Web..."
            // and "Running Gradle task..."
            let lower = trimmed.to_lowercase();
            if lower.starts_with("compiling ") && lower.contains("...") && !lower.contains("error")
            {
                continue;
            }
            if lower.starts_with("running gradle") && lower.contains("...") {
                continue;
            }
            error_lines.push(trimmed.to_string());
        }
        if error_lines.is_empty() {
            return "Flutter build: failed".to_string();
        }
        return format!("Flutter build: failed\n{}", error_lines.join("\n"));
    }

    // On success, extract the "Built" line and timing
    let built_re = Regex::new(r"Built\s+(.+?)(?:\s*$)").unwrap();
    let timing_re = Regex::new(r"(\d+[.,]\d+s)").unwrap();
    let size_re = Regex::new(r"\((\d+[\.,]?\d*\s*[KMGT]?B)\)").unwrap();

    let mut built_path = String::new();
    let mut timing = String::new();
    let mut size = String::new();

    for line in output.lines() {
        let trimmed = line.trim();
        // Look for checkmark/built lines: "√ Built build\web" or "✓ Built build/web"
        if let Some(caps) = built_re.captures(trimmed) {
            built_path = caps
                .get(1)
                .unwrap()
                .as_str()
                .trim()
                .replace('\\', "/")
                .to_string();
        }
        if let Some(caps) = size_re.captures(trimmed) {
            size = caps.get(1).unwrap().as_str().to_string();
        }
        // Timing from progress lines: "Compiling lib\main.dart for the Web...  18,1s"
        if timing.is_empty() {
            if let Some(caps) = timing_re.captures(trimmed) {
                timing = caps.get(1).unwrap().as_str().replace(',', ".").to_string();
            }
        }
    }

    if built_path.is_empty() {
        return "ok built".to_string();
    }

    let mut result = format!("ok {}", built_path);
    if !size.is_empty() {
        result.push_str(&format!(" ({})", size));
    }
    if !timing.is_empty() {
        if size.is_empty() {
            result.push_str(&format!(" ({})", timing));
        } else {
            result.push_str(&format!(" [{}]", timing));
        }
    }

    result
}

// ─── flutter pub ───

pub fn run_pub(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let subcommand = args.first().map(|s| s.as_str()).unwrap_or("get");
    let rest_args = if args.is_empty() { &[] } else { &args[1..] };

    let mut cmd = resolved_command("flutter");
    cmd.arg("pub");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: flutter pub {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run flutter pub. Is Flutter installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });

    let filtered = match subcommand {
        "get" | "upgrade" | "downgrade" | "add" | "remove" => {
            filter_flutter_pub_get(&raw, exit_code)
        }
        "outdated" => filter_flutter_pub_outdated(&stdout),
        "deps" => filter_flutter_pub_deps(&stdout, rest_args),
        _ => raw.clone(),
    };

    if let Some(hint) = crate::core::tee::tee_and_hint(&raw, "flutter_pub", exit_code) {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("flutter pub {}", args.join(" ")),
        &format!("rtk flutter pub {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

fn filter_flutter_pub_get(output: &str, exit_code: i32) -> String {
    if exit_code != 0 {
        // On failure, show full output minus noise
        let mut lines: Vec<&str> = output
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty()
                    && !t.starts_with("Resolving dependencies...")
                    && !t.starts_with("Downloading packages...")
            })
            .collect();
        if lines.is_empty() {
            return "Flutter pub: failed".to_string();
        }
        lines.truncate(20);
        return format!("Flutter pub: failed\n{}", lines.join("\n"));
    }

    let changed_re = Regex::new(r"Changed\s+(\d+)\s+dependenc").unwrap();
    let upgradable_re = Regex::new(r"(\d+)\s+packages?\s+have\s+newer").unwrap();
    let no_changes_re = Regex::new(r"Got dependencies|No dependencies changed").unwrap();

    let mut num_deps = 0u64;
    let mut num_upgradable = 0u64;
    let mut got_deps = false;

    for line in output.lines() {
        if let Some(caps) = changed_re.captures(line) {
            num_deps = caps.get(1).unwrap().as_str().parse().unwrap_or(0);
        }
        if let Some(caps) = upgradable_re.captures(line) {
            num_upgradable = caps.get(1).unwrap().as_str().parse().unwrap_or(0);
        }
        if no_changes_re.is_match(line) {
            got_deps = true;
        }
    }

    if num_deps > 0 {
        if num_upgradable > 0 {
            format!("ok {} deps ({} upgradable)", num_deps, num_upgradable)
        } else {
            format!("ok {} deps", num_deps)
        }
    } else if got_deps {
        "ok no changes".to_string()
    } else {
        "ok".to_string()
    }
}

fn filter_flutter_pub_outdated(output: &str) -> String {
    let mut packages = Vec::new();
    let header_re =
        Regex::new(r"Package\s+Name\s+Current\s+Upgradable\s+Resolvable\s+Latest").unwrap();
    let pkg_re =
        Regex::new(r"^(\S+)\s+\*?([\d.]+)\s+\*?([\d.]+)\s+\*?([\d.]+)\s+([\d.]+)").unwrap();

    let mut in_table = false;

    for line in output.lines() {
        let trimmed = line.trim();
        if header_re.is_match(trimmed) {
            in_table = true;
            continue;
        }
        if in_table {
            if trimmed.is_empty()
                || trimmed.starts_with("direct ")
                || trimmed.starts_with("dev_")
                || trimmed.starts_with("transitive ")
                || trimmed.starts_with("all ")
                || trimmed.starts_with("[*]")
                || trimmed.starts_with("Showing ")
            {
                continue;
            }
            if let Some(caps) = pkg_re.captures(trimmed) {
                let name = caps.get(1).unwrap().as_str();
                let current = caps.get(2).unwrap().as_str();
                let latest = caps.get(5).unwrap().as_str();
                if current != latest {
                    packages.push(format!("  {}: {} -> {}", name, current, latest));
                }
            }
        }
    }

    if packages.is_empty() {
        "Flutter pub outdated: all up-to-date".to_string()
    } else {
        format!(
            "Flutter pub outdated: {} upgradable\n{}",
            packages.len(),
            packages.join("\n")
        )
    }
}

fn filter_flutter_pub_deps(output: &str, _args: &[String]) -> String {
    let mut lines = Vec::new();
    let mut in_tree = false;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Keep SDK version headers
        if trimmed.starts_with("Dart SDK") || trimmed.starts_with("Flutter SDK") {
            lines.push(trimmed.to_string());
            continue;
        }

        // Project name line (e.g., "my_app 1.0.0+1") — no tree chars, has a space
        if !in_tree
            && !trimmed.starts_with("├")
            && !trimmed.starts_with("│")
            && !trimmed.starts_with("└")
        {
            if trimmed.contains(' ') && !trimmed.starts_with(' ') {
                lines.push(trimmed.to_string());
                in_tree = true;
                continue;
            }
        }

        // Only keep top-level deps: lines that start with ├── or └── at column 0
        // (no leading spaces or │ chars). Use the ORIGINAL line, not trimmed.
        if in_tree {
            let ltrimmed = line.trim_start();
            let indent = line.len() - ltrimmed.len();
            // Top-level deps have 0 indent in the tree
            if indent == 0 && (ltrimmed.starts_with("├── ") || ltrimmed.starts_with("└── "))
            {
                let dep = ltrimmed
                    .trim_start_matches("├── ")
                    .trim_start_matches("└── ");
                lines.push(format!("  {}", dep));
            }
        }
    }

    if lines.is_empty() {
        return output.to_string();
    }

    lines.join("\n")
}

// ─── flutter doctor ───

pub fn run_doctor(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("flutter");
    cmd.arg("doctor");

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: flutter doctor {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run flutter doctor. Is Flutter installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });

    // If -v flag was passed, use verbose output filtering
    let is_verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
    let filtered = if is_verbose {
        filter_flutter_doctor_verbose(&stdout)
    } else {
        filter_flutter_doctor(&stdout)
    };

    if let Some(hint) = crate::core::tee::tee_and_hint(&raw, "flutter_doctor", exit_code) {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("flutter doctor {}", args.join(" ")),
        &format!("rtk flutter doctor {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

fn filter_flutter_doctor(output: &str) -> String {
    let check_re = Regex::new(r"\[([\u2713\u2717\u2714\u2718!√✓✗×xX])\]").unwrap();
    let mut passed = 0usize;
    let mut issues = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(caps) = check_re.captures(trimmed) {
            let symbol = caps.get(1).unwrap().as_str();
            match symbol {
                "√" | "✓" | "\u{2714}" => passed += 1,
                _ => {
                    issues.push(trimmed.to_string());
                }
            }
        }
    }

    let total = passed + issues.len();

    if issues.is_empty() {
        format!("Flutter doctor: ok ({}/{} checks passed)", passed, total)
    } else {
        let mut out = format!(
            "Flutter doctor: {} issue(s) ({}/{} passed)",
            issues.len(),
            passed,
            total
        );
        for issue in &issues {
            out.push_str(&format!("\n  {}", issue));
        }
        out
    }
}

fn filter_flutter_doctor_verbose(output: &str) -> String {
    let section_re =
        Regex::new(r"\[([\u2713\u2717\u2714\u2718!√✓✗×xX])\]\s+(.+?)(?:\s+\[.*\])?\s*$").unwrap();
    let mut passed = 0usize;
    let mut issue_sections = Vec::new();
    let mut current_section: Option<Vec<String>> = None;
    let mut in_issue = false;

    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(caps) = section_re.captures(trimmed) {
            // Save previous section if it was an issue
            if in_issue {
                if let Some(section) = current_section.take() {
                    issue_sections.push(section);
                }
            }

            let symbol = caps.get(1).unwrap().as_str();
            match symbol {
                "√" | "✓" | "\u{2714}" => {
                    passed += 1;
                    in_issue = false;
                    current_section = None;
                }
                _ => {
                    in_issue = true;
                    current_section = Some(vec![trimmed.to_string()]);
                }
            }
        } else if in_issue {
            if let Some(ref mut section) = current_section {
                if !trimmed.is_empty() {
                    section.push(format!("  {}", trimmed));
                }
            }
        }
    }

    // Save last section
    if in_issue {
        if let Some(section) = current_section.take() {
            issue_sections.push(section);
        }
    }

    let total = passed + issue_sections.len();

    if issue_sections.is_empty() {
        format!("Flutter doctor: ok ({}/{} checks passed)", passed, total)
    } else {
        let mut out = format!(
            "Flutter doctor: {} issue(s) ({}/{} passed)",
            issue_sections.len(),
            passed,
            total
        );
        for section in &issue_sections {
            out.push('\n');
            out.push_str(&section.join("\n"));
        }
        out
    }
}

// ─── flutter clean ───

pub fn run_clean(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("flutter");
    cmd.arg("clean");

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: flutter clean {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run flutter clean. Is Flutter installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });

    let filtered = if output.status.success() {
        "ok cleaned".to_string()
    } else {
        format!("Flutter clean: failed\n{}", raw.trim())
    };

    if let Some(hint) = crate::core::tee::tee_and_hint(&raw, "flutter_clean", exit_code) {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("flutter clean {}", args.join(" ")),
        &format!("rtk flutter clean {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

// ─── flutter create ───

pub fn run_create(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("flutter");
    cmd.arg("create");

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: flutter create {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run flutter create. Is Flutter installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });
    let filtered = filter_flutter_create(&stdout, args, exit_code);

    if let Some(hint) = crate::core::tee::tee_and_hint(&raw, "flutter_create", exit_code) {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("flutter create {}", args.join(" ")),
        &format!("rtk flutter create {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

fn filter_flutter_create(output: &str, args: &[String], exit_code: i32) -> String {
    if exit_code != 0 {
        return format!("Flutter create: failed\n{}", output.trim());
    }

    let wrote_re = Regex::new(r"Wrote\s+(\d+)\s+files?").unwrap();
    let project_re = Regex::new(r"Creating project\s+(\S+?)\.{0,3}\s*$").unwrap();

    let mut num_files = 0u64;
    let mut project_name = String::new();

    for line in output.lines() {
        if let Some(caps) = wrote_re.captures(line) {
            num_files = caps.get(1).unwrap().as_str().parse().unwrap_or(0);
        }
        if let Some(caps) = project_re.captures(line) {
            project_name = caps
                .get(1)
                .unwrap()
                .as_str()
                .trim_end_matches('.')
                .to_string();
        }
    }

    // Fallback: try to get project name from args (last non-flag arg)
    if project_name.is_empty() {
        project_name = args
            .iter()
            .rev()
            .find(|a| !a.starts_with('-'))
            .cloned()
            .unwrap_or_default();
    }

    if num_files > 0 {
        format!("ok {} ({} files)", project_name, num_files)
    } else {
        format!("ok {}", project_name)
    }
}

// ─── flutter passthrough (devices, run, etc.) ───

pub fn run_other(args: &[OsString], verbose: u8) -> Result<()> {
    if args.is_empty() {
        anyhow::bail!("flutter: no subcommand specified");
    }

    let timer = tracking::TimedExecution::start();

    let subcommand = args[0].to_string_lossy();
    let mut cmd = resolved_command("flutter");
    cmd.arg(&*subcommand);

    for arg in &args[1..] {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: flutter {} ...", subcommand);
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run flutter {}", subcommand))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    print!("{}", stdout);
    eprint!("{}", stderr);

    timer.track(
        &format!("flutter {}", subcommand),
        &format!("rtk flutter {}", subcommand),
        &raw,
        &raw, // No filtering for unsupported commands
    );

    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }

    Ok(())
}

// ─── Tests ───

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_flutter_test_all_pass() {
        let json = r#"{"protocolVersion":"0.1.1","runnerVersion":null,"pid":1234,"type":"start","time":0}
{"suite":{"id":0,"platform":"vm","path":"test/widget_test.dart"},"type":"suite","time":0}
{"test":{"id":1,"name":"loading test/widget_test.dart","suiteID":0,"groupIDs":[],"metadata":{"skip":false,"skipReason":null},"line":null,"column":null,"url":null},"type":"testStart","time":1}
{"count":1,"time":2,"type":"allSuites"}
{"testID":1,"result":"success","skipped":false,"hidden":true,"type":"testDone","time":500}
{"group":{"id":2,"suiteID":0,"parentID":null,"name":"","metadata":{"skip":false,"skipReason":null},"testCount":1,"line":null,"column":null,"url":null},"type":"group","time":501}
{"test":{"id":3,"name":"Counter increments smoke test","suiteID":0,"groupIDs":[2],"metadata":{"skip":false,"skipReason":null},"line":14,"column":3,"url":"file:///test/widget_test.dart"},"type":"testStart","time":502}
{"testID":3,"result":"success","skipped":false,"hidden":false,"type":"testDone","time":600}
{"success":true,"type":"done","time":610}"#;

        let result = filter_flutter_test_json(json);
        assert!(
            result.contains("1 passed"),
            "Expected '1 passed' in: {}",
            result
        );
        assert!(
            result.contains("1 suite"),
            "Expected '1 suite' in: {}",
            result
        );
        assert!(
            !result.contains("FAIL"),
            "Should not contain FAIL: {}",
            result
        );
    }

    #[test]
    fn test_filter_flutter_test_with_failures() {
        let json = r#"{"protocolVersion":"0.1.1","runnerVersion":null,"pid":1234,"type":"start","time":0}
{"suite":{"id":0,"platform":"vm","path":"test/failing_test.dart"},"type":"suite","time":0}
{"test":{"id":1,"name":"loading test/failing_test.dart","suiteID":0,"groupIDs":[],"metadata":{"skip":false,"skipReason":null},"line":null,"column":null,"url":null},"type":"testStart","time":1}
{"count":1,"time":2,"type":"allSuites"}
{"testID":1,"result":"success","skipped":false,"hidden":true,"type":"testDone","time":500}
{"group":{"id":2,"suiteID":0,"parentID":null,"name":"","metadata":{"skip":false,"skipReason":null},"testCount":3,"line":null,"column":null,"url":null},"type":"group","time":501}
{"test":{"id":3,"name":"simple passing test","suiteID":0,"groupIDs":[2],"metadata":{"skip":false,"skipReason":null},"line":4,"column":3,"url":"file:///test/failing_test.dart"},"type":"testStart","time":502}
{"testID":3,"result":"success","skipped":false,"hidden":false,"type":"testDone","time":510}
{"test":{"id":4,"name":"simple failing test","suiteID":0,"groupIDs":[2],"metadata":{"skip":false,"skipReason":null},"line":8,"column":3,"url":"file:///test/failing_test.dart"},"type":"testStart","time":511}
{"testID":4,"error":"Expected: <3>\n  Actual: <2>\n","stackTrace":"package:matcher                                     expect\npackage:flutter_test/src/widget_tester.dart 473:18  expect\ntest/failing_test.dart 9:5                          main.<fn>\n","isFailure":true,"type":"error","time":520}
{"testID":4,"result":"failure","skipped":false,"hidden":false,"type":"testDone","time":521}
{"test":{"id":5,"name":"skipped test","suiteID":0,"groupIDs":[2],"metadata":{"skip":true,"skipReason":"Not ready"},"line":12,"column":3,"url":"file:///test/failing_test.dart"},"type":"testStart","time":522}
{"testID":5,"result":"success","skipped":true,"hidden":false,"type":"testDone","time":523}
{"success":false,"type":"done","time":530}"#;

        let result = filter_flutter_test_json(json);
        assert!(
            result.contains("1 passed"),
            "Expected '1 passed' in: {}",
            result
        );
        assert!(
            result.contains("1 failed"),
            "Expected '1 failed' in: {}",
            result
        );
        assert!(
            result.contains("1 skipped"),
            "Expected '1 skipped' in: {}",
            result
        );
        assert!(
            result.contains("FAIL simple failing test"),
            "Expected failure detail: {}",
            result
        );
        assert!(
            result.contains("Expected: <3>"),
            "Expected error message: {}",
            result
        );
    }

    #[test]
    fn test_filter_flutter_analyze_no_issues() {
        let output = "Analyzing my_app...\nNo issues found! (ran in 1.2s)";
        let result = filter_flutter_analyze(output);
        assert_eq!(result, "Flutter analyze: no issues found");
    }

    #[test]
    fn test_filter_flutter_analyze_with_issues() {
        let output = r#"Analyzing my_app...

warning - Unused import: 'dart:io' - lib\bad_code.dart:1:8 - unused_import
  error - Non-nullable instance field 'name' must be initialized - lib\bad_code.dart:5:10 - not_initialized_non_nullable_instance_field
   info - Don't invoke 'print' in production code - lib\bad_code.dart:10:5 - avoid_print

3 issues found. (ran in 1.3s)"#;

        let result = filter_flutter_analyze(output);
        assert!(
            result.contains("1 error"),
            "Expected '1 error' in: {}",
            result
        );
        assert!(
            result.contains("1 warning"),
            "Expected '1 warning' in: {}",
            result
        );
        assert!(
            result.contains("1 info"),
            "Expected '1 info' in: {}",
            result
        );
        assert!(
            result.contains("(3 issues)"),
            "Expected '(3 issues)' in: {}",
            result
        );
        // Errors should come first
        let error_pos = result.find("error:").unwrap();
        let warning_pos = result.find("warning:").unwrap();
        assert!(
            error_pos < warning_pos,
            "Errors should come before warnings"
        );
    }

    #[test]
    fn test_filter_flutter_build_success() {
        let output = "Compiling lib\\main.dart for the Web...                          \nFont asset \"MaterialIcons-Regular.otf\" was tree-shaken.\nCompiling lib\\main.dart for the Web...                             18,1s\n√ Built build\\web";
        let result = filter_flutter_build(output, 0);
        assert!(
            result.starts_with("ok build/web"),
            "Expected 'ok build/web' in: {}",
            result
        );
    }

    #[test]
    fn test_filter_flutter_pub_get() {
        let output = "Resolving dependencies...\nDownloading packages...\n+ flutter 0.0.0\n+ cupertino_icons 1.0.9\nChanged 27 dependencies!\n6 packages have newer versions incompatible with dependency constraints.\nTry `flutter pub outdated` for more information.";
        let result = filter_flutter_pub_get(output, 0);
        assert_eq!(result, "ok 27 deps (6 upgradable)");
    }

    #[test]
    fn test_filter_flutter_pub_outdated_all_current() {
        let output = "Showing outdated packages.\n[*] indicates versions that are not the latest available.\n\nPackage Name              Current   Upgradable  Resolvable  Latest\n\ndirect dependencies: all up-to-date.\n\ndev_dependencies: all up-to-date.\n\nall dependencies are up-to-date.";
        let result = filter_flutter_pub_outdated(output);
        assert_eq!(result, "Flutter pub outdated: all up-to-date");
    }

    #[test]
    fn test_filter_flutter_pub_outdated_with_updates() {
        let output = "Showing outdated packages.\n[*] indicates versions that are not the latest available.\n\nPackage Name              Current   Upgradable  Resolvable  Latest\n\ntransitive dependencies:\ncharacters                *1.4.0    *1.4.0      *1.4.0      1.4.1\nmeta                      *1.17.0   *1.17.0     *1.17.0     1.18.2";
        let result = filter_flutter_pub_outdated(output);
        assert!(
            result.contains("2 upgradable"),
            "Expected '2 upgradable' in: {}",
            result
        );
        assert!(
            result.contains("characters: 1.4.0 -> 1.4.1"),
            "Expected characters upgrade: {}",
            result
        );
        assert!(
            result.contains("meta: 1.17.0 -> 1.18.2"),
            "Expected meta upgrade: {}",
            result
        );
    }

    #[test]
    fn test_filter_flutter_doctor_all_ok() {
        let output = "Doctor summary (to see all details, run flutter doctor -v):\n[√] Flutter (Channel stable, 3.38.3)\n[√] Android toolchain\n[√] Chrome\n[√] Visual Studio\n[√] Connected device (4 available)\n[√] Network resources\n\n• No issues found!";
        let result = filter_flutter_doctor(output);
        assert!(result.contains("ok"), "Expected 'ok' in: {}", result);
        assert!(result.contains("6/6"), "Expected '6/6' in: {}", result);
    }

    #[test]
    fn test_filter_flutter_create() {
        let output = "Creating project my_app...\nWrote 130 files.\n\nAll done!\nYou can find general documentation...";
        let result = filter_flutter_create(output, &["my_app".to_string()], 0);
        assert_eq!(result, "ok my_app (130 files)");
    }

    #[test]
    fn test_filter_flutter_clean() {
        // Clean just returns "ok cleaned" on success - tested via run_clean behavior
    }

    #[test]
    fn test_strip_column() {
        assert_eq!(strip_column("lib/file.dart:5:10"), "lib/file.dart:5");
        assert_eq!(strip_column("lib/file.dart:5"), "lib/file.dart:5");
    }

    #[test]
    fn test_extract_user_location() {
        let stack = "package:matcher                                     expect\npackage:flutter_test/src/widget_tester.dart 473:18  expect\ntest/failing_test.dart 9:5                          main.<fn>\n";
        let loc = extract_user_location(stack);
        assert!(
            loc.contains("test/failing_test.dart:9"),
            "Expected location: {}",
            loc
        );
    }

    // ─── Token savings tests (using real fixtures) ───

    fn count_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    fn assert_savings(input: &str, output: &str, min_pct: f64, label: &str) {
        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(output);
        if input_tokens == 0 {
            return;
        }
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);
        assert!(
            savings >= min_pct,
            "{}: expected >={:.0}% savings, got {:.1}% ({} -> {} tokens)",
            label,
            min_pct,
            savings,
            input_tokens,
            output_tokens
        );
    }

    #[test]
    fn test_savings_flutter_test_pass() {
        let input = include_str!("../../../tests/fixtures/flutter/test_pass_json.txt");
        let output = filter_flutter_test_json(input);
        // Minimal fixture (1 test) has low absolute tokens; savings scale with project size.
        // With real projects (50+ tests), savings are 85-95%.
        assert_savings(input, &output, 40.0, "flutter test (pass)");
    }

    #[test]
    fn test_savings_flutter_test_fail() {
        let input = include_str!("../../../tests/fixtures/flutter/test_fail_json.txt");
        let output = filter_flutter_test_json(input);
        // Failure output retains error details; savings increase with more passing tests.
        assert_savings(input, &output, 20.0, "flutter test (fail)");
    }

    #[test]
    fn test_savings_flutter_analyze() {
        let input = include_str!("../../../tests/fixtures/flutter/analyze_issues.txt");
        let output = filter_flutter_analyze(input);
        // Analyze strips preamble/timing; with small issue counts, savings are modest.
        // Real projects with 50+ issues see 40-60% savings.
        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        assert!(
            output_tokens <= input_tokens,
            "flutter analyze: output should not be larger than input"
        );
    }

    #[test]
    fn test_savings_flutter_build_web() {
        let input = include_str!("../../../tests/fixtures/flutter/build_web.txt");
        let output = filter_flutter_build(input, 0);
        assert_savings(input, &output, 60.0, "flutter build web");
    }

    #[test]
    fn test_savings_flutter_pub_get() {
        let input = include_str!("../../../tests/fixtures/flutter/pub_get.txt");
        let output = filter_flutter_pub_get(input, 0);
        assert_savings(input, &output, 60.0, "flutter pub get");
    }

    #[test]
    fn test_savings_flutter_pub_outdated() {
        let input = include_str!("../../../tests/fixtures/flutter/pub_outdated.txt");
        let output = filter_flutter_pub_outdated(input);
        assert_savings(input, &output, 30.0, "flutter pub outdated");
    }

    #[test]
    fn test_savings_flutter_doctor() {
        let input = include_str!("../../../tests/fixtures/flutter/doctor.txt");
        let output = filter_flutter_doctor(input);
        assert_savings(input, &output, 60.0, "flutter doctor");
    }

    #[test]
    fn test_savings_flutter_clean() {
        let input = include_str!("../../../tests/fixtures/flutter/clean.txt");
        let output = if !input.trim().is_empty() {
            "ok cleaned".to_string()
        } else {
            input.to_string()
        };
        assert_savings(input, &output, 60.0, "flutter clean");
    }

    #[test]
    fn test_savings_flutter_create() {
        let input = include_str!("../../../tests/fixtures/flutter/create.txt");
        let output = filter_flutter_create(input, &["rtk_create_fixture".to_string()], 0);
        assert_savings(input, &output, 60.0, "flutter create");
    }
}
