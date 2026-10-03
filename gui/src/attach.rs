//! Files dropped on the window, attached to the next message the way the
//! command line's `-f` attaches them: whole, each in a `<file>` block after
//! the text, so the model needs no tool round to read them.

use std::fs;
use std::path::Path;

/// A file waiting to go with the next message.
#[derive(Debug)]
pub struct Attached {
    /// The path as the model sees it, with the home directory as `~`.
    pub path: String,
    /// The `<file>` block, ready to append to the message.
    pub block: String,
}

/// The last part of a path, for the chip that stands for the file.
pub fn name(path: &str) -> &str {
    path.rsplit('/').find(|part| !part.is_empty()).unwrap_or(path)
}

/// The same block as `nibble -f`.
fn block(path: &str, text: &str) -> String {
    format!("\n\n<file path=\"{path}\">\n{}\n</file>", text.trim_end())
}

/// Read a dropped file, refusing what `-f` refuses: folders, binary files,
/// and anything that would not fit whole, since an answer from half a file
/// would look just as confident as one from all of it.
pub fn read(path: &Path, room: usize) -> Result<Attached, String> {
    let shown = shown(path);
    let file = name(&shown).to_string();
    if path.is_dir() {
        return Err(format!("{file} is a folder; drop the files in it instead"));
    }
    let bytes = fs::read(path).map_err(|e| format!("Can't read {file}: {e}"))?;
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return Err(format!("{file} is not a text file"));
    }
    let block = block(&shown, &String::from_utf8_lossy(&bytes));
    if block.len() > room {
        return Err(format!(
            "{file} is too long to attach: {} characters, with room for {room} (Conversation kept, in Settings)",
            block.len()
        ));
    }
    Ok(Attached { path: shown, block })
}

/// The path as the model and the chips see it, with the home directory as `~`.
pub fn shown(path: &Path) -> String {
    let shown = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && shown.starts_with(&format!("{}/", home.trim_end_matches('/'))) => {
            format!("~{}", &shown[home.trim_end_matches('/').len()..])
        }
        _ => shown,
    }
}

/// Split a message as saved into what was typed and the paths of the files
/// that went with it, so the chat shows the files by name and not whole.
/// Chats from `nibble -f` carry the same blocks.
pub fn split(message: &str) -> (&str, Vec<&str>) {
    let mut text = message;
    let mut paths = Vec::new();
    while text.ends_with("\n</file>") {
        let Some(start) = text.rfind("\n\n<file path=\"") else { break };
        let rest = &text[start + "\n\n<file path=\"".len()..];
        let Some(end) = rest.find("\">\n") else { break };
        paths.push(&rest[..end]);
        text = &text[..start];
    }
    paths.reverse();
    (text, paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_go_as_nibble_f_sends_them_and_come_back_apart() {
        let dir = std::env::temp_dir().join(format!("nibble-gui-attach-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.rs"));
        fs::write(&a, "one\ntwo\n\n").unwrap();
        fs::write(&b, "fn main() {}\n").unwrap();

        let first = read(&a, 1000).unwrap();
        assert_eq!(first.block, format!("\n\n<file path=\"{}\">\none\ntwo\n</file>", a.display()));
        assert_eq!(name(&first.path), "a.txt");
        let second = read(&b, 1000).unwrap();

        let message = format!("What do these do?{}{}", first.block, second.block);
        let (text, paths) = split(&message);
        assert_eq!(text, "What do these do?");
        assert_eq!(paths, [a.display().to_string(), b.display().to_string()]);
        assert_eq!(split("No files here"), ("No files here", vec![]));

        assert!(read(&dir, 1000).unwrap_err().contains("folder"));
        assert!(read(&a, 10).unwrap_err().contains("too long"));
        fs::write(&b, [0u8, 1, 2]).unwrap();
        assert!(read(&b, 1000).unwrap_err().contains("not a text file"));
        assert!(read(&dir.join("missing"), 1000).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
