use std::fs;
use std::path::PathBuf;

const UPDATE_ENV: &str = "UPDATE_RENDER_FIXTURES";

pub fn assert_rows_match_fixture(name: &str, actual_rows: &[String]) {
    let actual = format!("{}\n", actual_rows.join("\n"));
    let fixture = fixture_path(name);

    if std::env::var_os(UPDATE_ENV).is_some() {
        if let Some(parent) = fixture.parent() {
            fs::create_dir_all(parent).expect("create fixture parent");
        }
        fs::write(&fixture, actual).expect("write fixture");
        return;
    }

    let expected = fs::read_to_string(&fixture).unwrap_or_else(|_| {
        panic!(
            "Missing fixture: {} (set {}=1 to generate)",
            fixture.display(),
            UPDATE_ENV
        )
    });

    assert_eq!(actual, expected.replace("\r\n", "\n"));
}

pub fn ansi_bytes_to_rows(bytes: &[u8], cols: usize, rows: usize) -> Vec<String> {
    let mut screen = vec![vec![' '; cols]; rows];
    let mut cursor_x = 0usize;
    let mut cursor_y = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\x1b' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                    if let Some((next, final_byte, params)) = parse_csi(bytes, i + 2) {
                        if final_byte == b'H' {
                            let (row, col) = parse_cursor_position(params);
                            cursor_y = row.min(rows.saturating_sub(1));
                            cursor_x = col.min(cols.saturating_sub(1));
                        }
                        if final_byte == b'J' && params == "2" {
                            for row in &mut screen {
                                for cell in row {
                                    *cell = ' ';
                                }
                            }
                        }
                        i = next;
                    } else {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            b'\r' => {
                cursor_x = 0;
                i += 1;
            }
            b'\n' => {
                cursor_y = (cursor_y + 1).min(rows.saturating_sub(1));
                i += 1;
            }
            _ => {
                let s = std::str::from_utf8(&bytes[i..]).expect("valid utf-8 render output");
                let ch = s.chars().next().expect("char exists");
                if cursor_y < rows && cursor_x < cols {
                    screen[cursor_y][cursor_x] = ch;
                }
                cursor_x = (cursor_x + 1).min(cols.saturating_sub(1));
                i += ch.len_utf8();
            }
        }
    }

    screen
        .into_iter()
        .map(|row| {
            row.into_iter()
                .collect::<String>()
                .trim_end_matches(' ')
                .to_string()
        })
        .collect()
}

fn parse_csi(bytes: &[u8], start: usize) -> Option<(usize, u8, &str)> {
    let mut idx = start;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if (0x40..=0x7e).contains(&byte) {
            let params = std::str::from_utf8(&bytes[start..idx]).ok()?;
            return Some((idx + 1, byte, params));
        }
        idx += 1;
    }
    None
}

fn parse_cursor_position(params: &str) -> (usize, usize) {
    let mut parts = params.split(';');
    let row_1_based = parts
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);
    let col_1_based = parts
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);
    (row_1_based.saturating_sub(1), col_1_based.saturating_sub(1))
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("render")
        .join(format!("{name}.txt"))
}
