//! Where rows come from.
//!
//! A [`RowSource`] hands out batches of [`Row`]. Two are supplied — a CSV/TSV reader and an
//! in-memory [`Frame`](crate::Frame) — and the trait is public so anything else that has
//! rows can feed the same loader.

use std::io::Read;

/// One row: its number, and its cells as `(column, value)`.
///
/// Values are strings because that is what a CSV holds. Typing them is the mapping's job —
/// `xsd:integer(?age)` says what the column means, and guessing here would mean a column of
/// postcodes silently becoming integers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// One-based row number, exposed to the mapping as `?ROWNUM`.
    pub number: u64,
    /// The cells, in column order.
    pub cells: Vec<(String, String)>,
}

/// Something that yields rows in batches.
pub trait RowSource {
    /// Returns up to `size` rows, numbering them from `start + 1`.
    ///
    /// An empty batch means the source is exhausted.
    ///
    /// # Errors
    ///
    /// Returns a description of whatever went wrong reading the underlying data.
    fn next_batch(&mut self, size: usize, start: u64) -> Result<Vec<Row>, String>;

    /// The column names, once they are known.
    fn columns(&self) -> Vec<String>;
}

/// How to read a delimited file.
#[derive(Debug, Clone)]
pub struct CsvOptions {
    /// Field separator.
    pub delimiter: u8,
    /// Whether the first row holds column names.
    ///
    /// With `false`, columns are named `col1`, `col2` … because a mapping has to have
    /// *something* to bind, and positional names are at least predictable.
    pub has_headers: bool,
    /// Rewrite headers into valid SPARQL variable names.
    ///
    /// A header like `First Name` or `total (£)` is not a variable, and a mapping cannot
    /// reference it. Normalising replaces every character that is not alphanumeric or `_`
    /// with `_`. A leading digit needs no special handling: SPARQL's `VARNAME` permits one,
/// so `?2024` is already a valid variable.
    pub normalize: bool,
    /// Allow rows with a different field count from the header.
    pub flexible: bool,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            has_headers: true,
            normalize: false,
            flexible: true,
        }
    }
}

impl CsvOptions {
    /// Tab-separated.
    #[must_use]
    pub fn tsv() -> Self {
        Self {
            delimiter: b'\t',
            ..Self::default()
        }
    }
}

/// Rewrites a header into something that can be a SPARQL variable.
///
/// Returns the name unchanged when it already is one, so a well-formed header is never
/// mangled by turning normalisation on.
#[must_use]
pub fn normalize_column(name: &str) -> String {
    if oxrdf::Variable::new(name).is_ok() {
        return name.to_owned();
    }
    let mut out: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// A CSV or TSV source.
pub struct Csv<R: Read> {
    reader: csv::Reader<R>,
    columns: Vec<String>,
}

impl<R: Read> Csv<R> {
    /// Opens a delimited source.
    ///
    /// # Errors
    ///
    /// Fails if the header row cannot be read.
    pub fn from_reader(inner: R, options: &CsvOptions) -> Result<Self, String> {
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(options.delimiter)
            .has_headers(options.has_headers)
            .flexible(options.flexible)
            .from_reader(inner);

        let columns = if options.has_headers {
            reader
                .headers()
                .map_err(|e| format!("reading the header row: {e}"))?
                .iter()
                .map(|h| {
                    if options.normalize {
                        normalize_column(h)
                    } else {
                        h.to_owned()
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

        Ok(Self { reader, columns })
    }
}

impl<R: Read> RowSource for Csv<R> {
    fn next_batch(&mut self, size: usize, start: u64) -> Result<Vec<Row>, String> {
        let mut out = Vec::with_capacity(size);
        let mut number = start;
        for record in self.reader.records().take(size) {
            let record = record.map_err(|e| format!("row {}: {e}", number + 1))?;
            number += 1;

            // Without headers the columns are positional, and are discovered from the
            // first row rather than declared.
            if self.columns.len() < record.len() {
                for i in self.columns.len()..record.len() {
                    self.columns.push(format!("col{}", i + 1));
                }
            }

            out.push(Row {
                number,
                cells: record
                    .iter()
                    .enumerate()
                    .map(|(i, value)| {
                        (
                            self.columns
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| format!("col{}", i + 1)),
                            value.to_owned(),
                        )
                    })
                    .collect(),
            });
        }
        Ok(out)
    }

    fn columns(&self) -> Vec<String> {
        self.columns.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(csv: &str, options: &CsvOptions) -> Vec<Row> {
        let mut source =
            Csv::from_reader(std::io::Cursor::new(csv.to_owned()), options).expect("open");
        let mut out = Vec::new();
        let mut n = 0;
        loop {
            let batch = source.next_batch(2, n).expect("batch");
            if batch.is_empty() {
                break;
            }
            n += batch.len() as u64;
            out.extend(batch);
        }
        out
    }

    #[test]
    fn headers_become_column_names_and_rows_are_numbered_from_one() {
        let rows = read_all("a,b\n1,2\n3,4\n", &CsvOptions::default());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].number, 1, "the first data row is 1, not 0");
        assert_eq!(rows[1].number, 2);
        assert_eq!(rows[0].cells[0], ("a".to_owned(), "1".to_owned()));
    }

    #[test]
    fn batching_does_not_lose_or_duplicate_rows() {
        // Batch size 2 over 5 rows: the boundary is where an off-by-one would show.
        let rows = read_all("a\n1\n2\n3\n4\n5\n", &CsvOptions::default());
        let values: Vec<String> = rows.iter().map(|r| r.cells[0].1.clone()).collect();
        assert_eq!(values, vec!["1", "2", "3", "4", "5"]);
        let numbers: Vec<u64> = rows.iter().map(|r| r.number).collect();
        assert_eq!(numbers, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn tabs_are_a_delimiter_option() {
        let rows = read_all("a\tb\n1\t2\n", &CsvOptions::tsv());
        assert_eq!(rows[0].cells.len(), 2);
        assert_eq!(rows[0].cells[1].1, "2");
    }

    #[test]
    fn normalisation_makes_a_header_usable_as_a_variable() {
        assert_eq!(normalize_column("First Name"), "First_Name");
        assert_eq!(normalize_column("total (£)"), "total____");
        // SPARQL's VARNAME explicitly permits a leading digit, so `?2024` is already a
        // valid variable and normalising must leave it alone rather than "fixing" it.
        assert_eq!(normalize_column("2024"), "2024");
        // Already valid, so left exactly as it is.
        assert_eq!(normalize_column("already_fine"), "already_fine");
        for name in ["First Name", "total (£)", "2024", ""] {
            assert!(
                oxrdf::Variable::new(normalize_column(name)).is_ok(),
                "normalising {name:?} did not produce a variable"
            );
        }
    }

    #[test]
    fn an_empty_cell_is_preserved_as_empty_not_dropped() {
        // The mapping turns empty into UNDEF; the source's job is to report it faithfully
        // rather than decide.
        let rows = read_all("a,b\n1,\n", &CsvOptions::default());
        assert_eq!(rows[0].cells[1], ("b".to_owned(), String::new()));
    }

    #[test]
    fn without_headers_columns_are_positional() {
        let options = CsvOptions {
            has_headers: false,
            ..CsvOptions::default()
        };
        let rows = read_all("1,2\n3,4\n", &options);
        assert_eq!(rows.len(), 2, "the first line is data, not a header");
        assert_eq!(rows[0].cells[0].0, "col1");
        assert_eq!(rows[0].cells[1].0, "col2");
    }

    #[test]
    fn a_short_row_keeps_the_columns_it_has() {
        let rows = read_all("a,b,c\n1,2\n", &CsvOptions::default());
        assert_eq!(rows[0].cells.len(), 2, "no phantom third cell");
    }
}
