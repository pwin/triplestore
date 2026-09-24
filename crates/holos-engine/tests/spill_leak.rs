//! Scratch: is the scratch directory removed after a merge, with readers open?
use holos_engine::spill::Distinct;
use oxrdf::{Literal, Term, Variable};
use spareval::QuerySolution;
use std::sync::Arc;

fn variables() -> Arc<[Variable]> {
    Arc::from(vec![Variable::new_unchecked("a"), Variable::new_unchecked("b")])
}

#[test]
fn merged_runs_are_cleaned_up() {
    let scratch = {
        let mut distinct = Distinct::new(2, 16 * 1024).expect("collector");
        for i in 0..20_000_u32 {
            let row: Vec<Option<Term>> = vec![
                Some(Literal::new_simple_literal(format!("value number {i}")).into()),
                Some(Literal::new_simple_literal(format!("second column {i}")).into()),
            ];
            let solution: QuerySolution = (variables(), row).into();
            distinct.push(&solution).expect("push");
        }
        assert!(distinct.runs() > 1, "nothing spilled");
        let dir = distinct.scratch().to_path_buf();
        let mut merged = distinct.merge().expect("merge");
        let mut n = 0;
        while merged.next_row().expect("row").is_some() {
            n += 1;
        }
        assert_eq!(n, 20_000);
        assert!(dir.exists(), "still open here");
        drop(merged);
        dir
    };
    let left: Vec<_> = std::fs::read_dir(&scratch)
        .map(|d| d.filter_map(Result::ok).map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(
        !scratch.exists(),
        "{} left behind, holding {:?}",
        scratch.display(),
        left
    );
}
