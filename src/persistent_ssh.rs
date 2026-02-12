use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Persistent SSH session using a single ssh.exe process.
/// One handshake, many commands.
///
/// Protocol:
///   EXISTS <path>\n  -> OK | NO
///   CAT <path>\n     -> OK <len>\n <raw bytes>
///   QUIT
pub struct PersistentSsh {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl PersistentSsh {
    pub fn connect(user_host: &str) -> Result<Self> {
        let mut child = Command::new("ssh")
            .args([
                "-p",
                "58022",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "PreferredAuthentications=publickey",
                "-o",
                "PasswordAuthentication=no",
                "-o",
                "KbdInteractiveAuthentication=no",
                "-o",
                "GSSAPIAuthentication=no",
                user_host,
                "sh",
                "-lc",
                REMOTE_LOOP,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to start ssh to {user_host}"))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("ssh stdin missing"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("ssh stdout missing"))?;

        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    pub fn exists(&mut self, path: &str) -> Result<bool> {
        self.write_line(&format!("EXISTS {}", sanitize(path)))?;
        match self.read_line()?.as_str() {
            "OK" => Ok(true),
            "NO" => Ok(false),
            other => Err(anyhow!("Unexpected EXISTS response: {other}")),
        }
    }

    pub fn cat(&mut self, path: &str) -> Result<Vec<u8>> {
        self.write_line(&format!("CAT {}", sanitize(path)))?;
        let header = self.read_line()?;
        if header == "NO" {
            return Err(anyhow!("Remote file not found: {path}"));
        }
        let len = parse_len(&header)?;
        let mut buf = vec![0u8; len];
        self.read_exact(&mut buf)?;
        Ok(buf)
    }

    pub fn close(mut self) {
        let _ = self.write_line("QUIT");
        let _ = self.child.kill();
    }

    fn write_line(&mut self, s: &str) -> Result<()> {
        self.stdin.write_all(s.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush().ok();
        Ok(())
    }

    fn read_line(&mut self) -> Result<String> {
        let mut out = Vec::new();
        loop {
            let mut b = [0u8; 1];
            let n = self.stdout.read(&mut b)?;
            if n == 0 {
                return Err(anyhow!("ssh session closed"));
            }
            if b[0] == b'\n' {
                break;
            }
            out.push(b[0]);
            if out.len() > 8192 {
                return Err(anyhow!("header too long"));
            }
        }
        Ok(String::from_utf8(out)?.trim_end().to_string())
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.stdout.read_exact(buf)?;
        Ok(())
    }
}

fn sanitize(p: &str) -> String {
    p.replace('\n', "").replace('\r', "")
}

fn parse_len(h: &str) -> Result<usize> {
    let mut it = h.split_whitespace();
    if it.next() != Some("OK") {
        return Err(anyhow!("Unexpected header: {h}"));
    }
    let n = it.next().ok_or_else(|| anyhow!("Missing length"))?;
    Ok(n.parse()?)
}

const REMOTE_LOOP: &str = r#"
set -eu
while IFS= read -r line; do
  cmd=${line%% *}
  arg=${line#* }
  case "$cmd" in
    QUIT)
      exit 0
      ;;
    EXISTS)
      [ "$arg" != "$line" ] && [ -f "$arg" ] && echo OK || echo NO
      ;;
    CAT)
      if [ "$arg" != "$line" ] && [ -f "$arg" ]; then
        n=$(wc -c < "$arg" | tr -d '[:space:]')
        echo "OK $n"
        cat -- "$arg"
      else
        echo NO
      fi
      ;;
    *)
      echo NO
      ;;
  esac
done
"#;
