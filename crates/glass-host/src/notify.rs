//! Talking to the person at the computer: notifications, yes/no prompts and a text box.
//! macOS uses AppleScript (works from the background service, no app bundle needed);
//! Linux uses notify-send, with zenity or kdialog for prompts; Windows uses PowerShell (a
//! toast, a message box, an input box). Test mode never shows any.

use std::time::Duration;

/// A notification; fire and forget.
pub fn notify(title: &str, body: &str) {
    if cfg!(test) || crate::testmode::enabled() {
        crate::testmode::record_notification(title, body);
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification {} with title {}",
            applescript_string(body),
            applescript_string(title)
        );
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("notify-send")
            .args(["--app-name=Glass", title, body])
            .spawn();
    }
    #[cfg(windows)]
    {
        use crate::platform::ps_string;
        // A toast under PowerShell's own app id: an unpackaged app has none of its own.
        let script = format!(
            "$m = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]; \
             $x = $m::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
             $t = $x.GetElementsByTagName('text'); \
             [void]$t.Item(0).AppendChild($x.CreateTextNode({})); \
             [void]$t.Item(1).AppendChild($x.CreateTextNode({})); \
             $m::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show([Windows.UI.Notifications.ToastNotification]::new($x))",
            ps_string(title),
            ps_string(body)
        );
        let _ = crate::platform::powershell(&script).spawn();
    }
}

/// Asks a yes/no question; `Some(true)` for `yes`, `Some(false)` for `no`, `None` if
/// nobody answered within `timeout` or no prompt could be shown.
pub async fn confirm(title: &str, body: &str, yes: &str, no: &str, timeout: Duration) -> Option<bool> {
    if cfg!(test) || crate::testmode::enabled() {
        return None; // tests decide through the approval queue instead
    }
    let secs = timeout.as_secs().max(1);
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display dialog {} with title {} buttons {{{}, {}}} default button {} cancel button {} giving up after {secs} with icon caution",
            applescript_string(body),
            applescript_string(title),
            applescript_string(no),
            applescript_string(yes),
            applescript_string(yes),
            applescript_string(no),
        );
        let out = tokio::process::Command::new("osascript")
            .args(["-e", &script])
            .output()
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("gave up:true") {
            return None;
        }
        // The cancel button exits non-zero ("User canceled").
        return Some(out.status.success() && text.contains(&format!("button returned:{yes}")));
    }
    #[cfg(target_os = "linux")]
    {
        if crate::platform::has_command("zenity") {
            let status = tokio::process::Command::new("zenity")
                .args([
                    "--question",
                    &format!("--title={title}"),
                    &format!("--text={body}"),
                    &format!("--ok-label={yes}"),
                    &format!("--cancel-label={no}"),
                    &format!("--timeout={secs}"),
                ])
                .status()
                .await
                .ok()?;
            return match status.code() {
                Some(0) => Some(true),
                Some(1) => Some(false),
                _ => None, // 5: timed out
            };
        }
        if crate::platform::has_command("notify-send") {
            // libnotify 0.7.9+: actions, printed when clicked.
            let child = tokio::process::Command::new("notify-send")
                .args([
                    "--app-name=Glass",
                    "--urgency=critical",
                    "--wait",
                    &format!("--action=yes={yes}"),
                    &format!("--action=no={no}"),
                    title,
                    body,
                ])
                .kill_on_drop(true)
                .output();
            let out = tokio::time::timeout(timeout, child).await.ok()?.ok()?;
            return match String::from_utf8_lossy(&out.stdout).trim() {
                "yes" => Some(true),
                "no" => Some(false),
                _ => None,
            };
        }
        let _ = (title, body, yes, no, secs);
        None
    }
    #[cfg(windows)]
    {
        use crate::platform::ps_string;
        // Yes/No, a question icon, on top of other windows (MB_SYSTEMMODAL); -1 on timeout.
        let script = format!(
            "(New-Object -ComObject WScript.Shell).Popup({}, {secs}, {}, 4 + 32 + 4096)",
            ps_string(&format!("{body}\n\nYes: {yes}\nNo: {no}")),
            ps_string(title),
        );
        let out = tokio::process::Command::from(crate::platform::powershell(&script))
            .output()
            .await
            .ok()?;
        match String::from_utf8_lossy(&out.stdout).trim() {
            "6" => Some(true),
            "7" => Some(false),
            _ => None,
        }
    }
}

/// Asks for a line of text (e.g. a chat reply); `None` if cancelled.
pub async fn ask_text(title: &str, prompt: &str) -> Option<String> {
    if crate::testmode::enabled() {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "text returned of (display dialog {} with title {} default answer \"\" buttons {{\"Cancel\", \"Send\"}} default button \"Send\")",
            applescript_string(prompt),
            applescript_string(title)
        );
        let out = tokio::process::Command::new("osascript")
            .args(["-e", &script])
            .output()
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return (out.status.success() && !text.is_empty()).then_some(text);
    }
    #[cfg(target_os = "linux")]
    {
        for (cmd, args) in [
            ("zenity", vec!["--entry".to_string(), format!("--title={title}"), format!("--text={prompt}")]),
            ("kdialog", vec!["--title".to_string(), title.to_string(), "--inputbox".to_string(), prompt.to_string()]),
        ] {
            if crate::platform::has_command(cmd) {
                let out = tokio::process::Command::new(cmd).args(&args).output().await.ok()?;
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                return (out.status.success() && !text.is_empty()).then_some(text);
            }
        }
        None
    }
    #[cfg(windows)]
    {
        use crate::platform::ps_string;
        let script = format!(
            "[Console]::OutputEncoding = [Text.Encoding]::UTF8; Add-Type -AssemblyName Microsoft.VisualBasic; \
             [Microsoft.VisualBasic.Interaction]::InputBox({}, {})",
            ps_string(prompt),
            ps_string(title)
        );
        let out = tokio::process::Command::from(crate::platform::powershell(&script))
            .output()
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !text.is_empty()).then_some(text)
    }
}

/// An AppleScript string literal.
pub fn applescript_string(s: &str) -> String {
    let escaped: String = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    #[test]
    fn applescript_strings_are_escaped() {
        assert_eq!(super::applescript_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(super::applescript_string("x\u{7}y"), "\"xy\"");
    }
}
