use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub const DEFAULT_PG_SOCKET: &str = "/run/postgresql/.s.PGSQL.5432";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveDbExecution {
    pub namespace: String,
    pub workflow_id: String,
    pub run_id_hex: String,
}

pub fn query_running_executions(
    socket_path: &Path,
    db_user: &str,
    db_name: &str,
) -> Result<Vec<ActiveDbExecution>, String> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| format!("connect postgres {}: {e}", socket_path.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;

    send_startup(&mut stream, db_user, db_name)?;
    wait_for_ready(&mut stream)?;

    let sql = "SELECT n.name, c.workflow_id, encode(c.run_id, 'hex') \
               FROM current_executions c \
               JOIN namespaces n ON n.id = c.namespace_id \
               WHERE c.status IN (1, 8) \
                 AND n.name <> 'temporal-system' \
                 AND c.workflow_id NOT LIKE 'temporal-sys-%';";
    send_simple_query(&mut stream, sql)?;
    let rows = read_query_rows(&mut stream)?;
    let _ = send_terminate(&mut stream);

    let mut out = Vec::with_capacity(rows.len());
    for mut cols in rows {
        if cols.len() != 3 {
            return Err("unexpected column count from current_executions".into());
        }
        let run_id_hex = cols.pop().unwrap();
        let workflow_id = cols.pop().unwrap();
        let namespace = cols.pop().unwrap();
        out.push(ActiveDbExecution {
            namespace,
            workflow_id,
            run_id_hex,
        });
    }
    Ok(out)
}

pub fn format_uuid_hex(hex: &str) -> String {
    if hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )
    } else {
        hex.to_owned()
    }
}

fn send_startup<W: Write>(stream: &mut W, user: &str, database: &str) -> Result<(), String> {
    let mut body = Vec::new();
    body.extend_from_slice(&196608u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.extend_from_slice(b"database\0");
    body.extend_from_slice(database.as_bytes());
    body.push(0);
    body.push(0);

    let total_len = (body.len() + 4) as u32;
    stream
        .write_all(&total_len.to_be_bytes())
        .and_then(|_| stream.write_all(&body))
        .and_then(|_| stream.flush())
        .map_err(|e| format!("send postgres startup: {e}"))
}

fn send_simple_query<W: Write>(stream: &mut W, sql: &str) -> Result<(), String> {
    let len = (sql.len() + 5) as u32;
    stream
        .write_all(b"Q")
        .and_then(|_| stream.write_all(&len.to_be_bytes()))
        .and_then(|_| stream.write_all(sql.as_bytes()))
        .and_then(|_| stream.write_all(&[0]))
        .and_then(|_| stream.flush())
        .map_err(|e| format!("send postgres query: {e}"))
}

fn send_terminate<W: Write>(stream: &mut W) -> io::Result<()> {
    stream.write_all(b"X\x00\x00\x00\x04")?;
    stream.flush()
}

fn read_msg<R: Read>(stream: &mut R) -> Result<(u8, Vec<u8>), String> {
    let mut hdr = [0u8; 5];
    stream
        .read_exact(&mut hdr)
        .map_err(|e| format!("read postgres header: {e}"))?;
    let tag = hdr[0];
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    if !(4..=4 * 1024 * 1024).contains(&len) {
        return Err(format!("invalid postgres packet length {len}"));
    }
    let mut payload = vec![0u8; len - 4];
    stream
        .read_exact(&mut payload)
        .map_err(|e| format!("read postgres payload: {e}"))?;
    Ok((tag, payload))
}

fn wait_for_ready<R: Read>(stream: &mut R) -> Result<(), String> {
    loop {
        let (tag, payload) = read_msg(stream)?;
        match tag {
            b'R' => {
                if payload.len() < 4 {
                    return Err("truncated Authentication packet".into());
                }
                let code = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                if code != 0 {
                    return Err(format!(
                        "postgres peer authentication not accepted (code {code})"
                    ));
                }
            }
            b'S' | b'K' | b'N' => {}
            b'Z' => return Ok(()),
            b'E' => {
                return Err(format!(
                    "postgres startup error: {}",
                    parse_pg_error(&payload)
                ));
            }
            other => {
                return Err(format!(
                    "unexpected postgres startup tag '{}'",
                    other as char
                ));
            }
        }
    }
}

fn read_query_rows<R: Read>(stream: &mut R) -> Result<Vec<Vec<String>>, String> {
    let mut rows = Vec::new();
    let mut query_err = None;
    loop {
        let (tag, payload) = read_msg(stream)?;
        match tag {
            b'T' | b'C' | b'N' | b'S' => {}
            b'D' => rows.push(parse_data_row(&payload)?),
            b'E' => query_err = Some(parse_pg_error(&payload)),
            b'Z' => {
                if let Some(err) = query_err {
                    return Err(format!("postgres query error: {err}"));
                }
                return Ok(rows);
            }
            other => return Err(format!("unexpected postgres query tag '{}'", other as char)),
        }
    }
}

fn parse_data_row(payload: &[u8]) -> Result<Vec<String>, String> {
    if payload.len() < 2 {
        return Err("truncated DataRow".into());
    }
    let ncols = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    let mut pos = 2usize;
    let mut out = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        if pos + 4 > payload.len() {
            return Err("truncated DataRow column length".into());
        }
        let clen = i32::from_be_bytes([
            payload[pos],
            payload[pos + 1],
            payload[pos + 2],
            payload[pos + 3],
        ]);
        pos += 4;
        if clen < 0 {
            out.push(String::new());
            continue;
        }
        let ulen = clen as usize;
        if pos + ulen > payload.len() {
            return Err("truncated DataRow column bytes".into());
        }
        let s = std::str::from_utf8(&payload[pos..pos + ulen])
            .map_err(|_| "non-UTF-8 DataRow value")?;
        out.push(s.to_owned());
        pos += ulen;
    }
    Ok(out)
}

fn parse_pg_error(payload: &[u8]) -> String {
    let mut pos = 0usize;
    let mut msg = String::new();
    while pos < payload.len() && payload[pos] != 0 {
        let field_type = payload[pos];
        pos += 1;
        let end = payload[pos..]
            .iter()
            .position(|b| *b == 0)
            .map_or(payload.len(), |i| pos + i);
        if field_type == b'M' {
            msg = String::from_utf8_lossy(&payload[pos..end]).into_owned();
        }
        pos = (end + 1).min(payload.len());
    }
    if msg.is_empty() {
        "database error".into()
    } else {
        msg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn test_pg_wire_client_parses_rows_and_surfaces_errors() {
        let sock = std::env::temp_dir().join(format!("boreal-pg-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut len_buf = [0u8; 4];
            conn.read_exact(&mut len_buf).unwrap();
            let slen = u32::from_be_bytes(len_buf) as usize;
            let mut startup = vec![0u8; slen - 4];
            conn.read_exact(&mut startup).unwrap();
            // AuthenticationOk + ReadyForQuery
            conn.write_all(b"R\x00\x00\x00\x08\x00\x00\x00\x00Z\x00\x00\x00\x05I")
                .unwrap();

            let (qtag, _) = read_msg(&mut conn).unwrap();
            assert_eq!(qtag, b'Q');

            // One DataRow: ("alice", "wf-1", "00112233445566778899aabbccddeeff")
            let mut row = Vec::new();
            row.extend_from_slice(&3u16.to_be_bytes());
            for col in ["alice", "wf-1", "00112233445566778899aabbccddeeff"] {
                row.extend_from_slice(&(col.len() as i32).to_be_bytes());
                row.extend_from_slice(col.as_bytes());
            }
            conn.write_all(b"D").unwrap();
            conn.write_all(&((row.len() + 4) as u32).to_be_bytes())
                .unwrap();
            conn.write_all(&row).unwrap();
            conn.write_all(b"C\x00\x00\x00\x0dSELECT 1\0Z\x00\x00\x00\x05I")
                .unwrap();
        });

        let active = query_running_executions(&sock, "boreal-broker", "temporal").unwrap();
        server.join().unwrap();
        let _ = std::fs::remove_file(&sock);

        assert_eq!(
            active,
            vec![ActiveDbExecution {
                namespace: "alice".into(),
                workflow_id: "wf-1".into(),
                run_id_hex: "00112233445566778899aabbccddeeff".into(),
            }]
        );
        assert_eq!(
            format_uuid_hex(&active[0].run_id_hex),
            "00112233-4455-6677-8899-aabbccddeeff"
        );
    }
}
