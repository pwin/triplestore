//! An in-memory frame, and the Polars bridge.
//!
//! [`Frame`] is columns plus rows of strings. It exists so that *anything* holding tabular
//! data can drive the loader without this crate knowing about it: a Polars `DataFrame`, a
//! pandas frame arriving through the Python binding, a database cursor, a `Vec` built by
//! hand in a test.
//!
//! With the `polars` feature on, [`Frame::from_polars`] converts a `DataFrame` directly.
//! The feature is off by default because Polars is a large dependency tree and the CSV path
//! covers most uses; a caller who already has a `DataFrame` in hand turns it on.

use crate::source::{Row, RowSource};

/// Tabular data held in memory.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Frame {
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
    cursor: usize,
}

impl Frame {
    /// A frame from columns and rows.
    ///
    /// A row shorter than the column list contributes only the cells it has; a row longer
    /// than it has the extra cells ignored, because there is no column name to bind them
    /// to and inventing one would be a guess.
    #[must_use]
    pub fn new(columns: Vec<String>, rows: Vec<Vec<String>>) -> Self {
        Self {
            columns,
            rows,
            cursor: 0,
        }
    }

    /// A frame from column-oriented data, which is how a dataframe is usually shaped.
    ///
    /// # Errors
    ///
    /// Fails if the columns are not all the same length, because that is not a table and
    /// silently truncating to the shortest would lose data without saying so.
    pub fn from_columns(columns: Vec<(String, Vec<String>)>) -> Result<Self, String> {
        let height = columns.first().map_or(0, |(_, values)| values.len());
        for (name, values) in &columns {
            if values.len() != height {
                return Err(format!(
                    "column `{name}` has {} values but the frame is {height} rows",
                    values.len()
                ));
            }
        }
        let names: Vec<String> = columns.iter().map(|(n, _)| n.clone()).collect();
        let rows = (0..height)
            .map(|i| columns.iter().map(|(_, v)| v[i].clone()).collect())
            .collect();
        Ok(Self::new(names, rows))
    }

    /// Rows in the frame.
    #[must_use]
    pub fn height(&self) -> usize {
        self.rows.len()
    }

    /// Whether the frame holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Converts a Polars `DataFrame`.
    ///
    /// Every column is rendered to its string form, which is the same thing a CSV export
    /// would do. Typing is then the mapping's decision — `xsd:integer(?age)` — rather than
    /// this function's, so a load from a frame and a load from the CSV of that frame
    /// produce the same triples.
    ///
    /// # Errors
    ///
    /// Fails if a column cannot be rendered.
    #[cfg(feature = "polars")]
    pub fn from_polars(frame: &polars::prelude::DataFrame) -> Result<Self, String> {
        use polars::prelude::*;

        let mut columns = Vec::with_capacity(frame.width());
        for series in frame.columns() {
            let name = series.name().to_string();
            let mut values = Vec::with_capacity(frame.height());
            for i in 0..frame.height() {
                let value = series
                    .get(i)
                    .map_err(|e| format!("column `{name}` row {i}: {e}"))?;
                values.push(match value {
                    AnyValue::Null => String::new(),
                    // `to_string` on a string AnyValue includes the quotes; the inner text
                    // is what a cell holds.
                    AnyValue::String(s) => s.to_owned(),
                    AnyValue::StringOwned(s) => s.to_string(),
                    other => other.to_string(),
                });
            }
            columns.push((name, values));
        }
        Self::from_columns(columns)
    }
}

impl RowSource for Frame {
    fn next_batch(&mut self, size: usize, start: u64) -> Result<Vec<Row>, String> {
        let end = (self.cursor + size).min(self.rows.len());
        let batch = (self.cursor..end)
            .map(|i| Row {
                number: start + (i - self.cursor) as u64 + 1,
                cells: self.rows[i]
                    .iter()
                    .enumerate()
                    .filter_map(|(c, value)| {
                        self.columns
                            .get(c)
                            .map(|name| (name.clone(), value.clone()))
                    })
                    .collect(),
            })
            .collect();
        self.cursor = end;
        Ok(batch)
    }

    fn columns(&self) -> Vec<String> {
        self.columns.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Frame {
        Frame::new(
            vec!["id".into(), "name".into()],
            vec![
                vec!["1".into(), "Alice".into()],
                vec!["2".into(), "Bob".into()],
                vec!["3".into(), "Carol".into()],
            ],
        )
    }

    #[test]
    fn rows_come_out_numbered_and_named() {
        let mut f = frame();
        let batch = f.next_batch(10, 0).expect("batch");
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].number, 1);
        assert_eq!(batch[2].cells[1], ("name".to_owned(), "Carol".to_owned()));
    }

    #[test]
    fn batching_walks_the_whole_frame_exactly_once() {
        let mut f = frame();
        let mut seen = Vec::new();
        let mut n = 0;
        loop {
            let batch = f.next_batch(2, n).expect("batch");
            if batch.is_empty() {
                break;
            }
            n += batch.len() as u64;
            seen.extend(batch);
        }
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "numbering must be continuous across batches"
        );
    }

    #[test]
    fn column_oriented_input_transposes() {
        let f = Frame::from_columns(vec![
            ("a".into(), vec!["1".into(), "2".into()]),
            ("b".into(), vec!["x".into(), "y".into()]),
        ])
        .expect("frame");
        assert_eq!(f.height(), 2);
        let mut f = f;
        let batch = f.next_batch(10, 0).expect("batch");
        assert_eq!(
            batch[0].cells,
            vec![
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "x".to_owned())
            ]
        );
    }

    #[test]
    fn ragged_columns_are_refused() {
        // Truncating to the shortest column would lose data with no indication.
        let outcome = Frame::from_columns(vec![
            ("a".into(), vec!["1".into(), "2".into()]),
            ("b".into(), vec!["x".into()]),
        ]);
        assert!(outcome.is_err());
        assert!(format!("{}", outcome.unwrap_err()).contains("column `b`"));
    }

    #[test]
    fn an_empty_frame_yields_no_rows() {
        let mut f = Frame::default();
        assert!(f.next_batch(10, 0).expect("batch").is_empty());
    }
}
