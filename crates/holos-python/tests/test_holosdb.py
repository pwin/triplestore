"""Tests for the Python bindings.

Run against an installed wheel:

    maturin develop --release
    python -m pytest crates/holos-python/tests -v
"""

import os
import tempfile

import pytest

import holosdb
from holosdb import Policy, Principal, Store

EX = "http://example.com/"
HR_TRIG = os.path.join(
    os.path.dirname(__file__), "..", "..", "..", "examples", "hr.trig"
)


@pytest.fixture
def store():
    s = Store()
    s.load(HR_TRIG)
    return s


# --------------------------------------------------------------------------- basics


def test_the_wheel_reports_what_it_contains():
    # A Python extra cannot toggle compiled code, so this is the only honest way to ask.
    assert isinstance(holosdb.has_rocksdb(), bool)
    assert holosdb.__version__


def test_load_and_count(store):
    assert len(store) == 21
    assert store.dictionary_size > 0
    assert not store.is_persistent


def test_named_graphs(store):
    graphs = store.named_graphs()
    assert len(graphs) == 3
    assert any("public" in g for g in graphs)


def test_the_dictionary_is_smaller_than_the_term_count(store):
    # Integers and short strings are inlined into their ids and never reach the
    # dictionary, so this inequality is the encoding working rather than a coincidence.
    assert store.dictionary_size < len(store) * 3


# --------------------------------------------------------------------------- querying


def test_select_returns_solutions(store):
    rows = store.query(f"SELECT ?n WHERE {{ GRAPH ?g {{ ?s <{EX}name> ?n }} }}")
    assert {r["n"] for r in rows} == {"Alice", "Bob", "Carol"}


def test_ask_returns_a_bool(store):
    assert store.query("ASK { GRAPH ?g { ?s ?p ?o } }") is True
    assert store.query(f"ASK {{ ?s <{EX}nothing> ?o }}") is False


def test_construct_returns_triples(store):
    out = store.query(
        f"CONSTRUCT {{ ?s ?p ?o }} WHERE {{ GRAPH ?g {{ ?s <{EX}name> ?o . BIND(<{EX}name> AS ?p) }} }}"
    )
    assert isinstance(out, list)
    assert all(isinstance(t, str) for t in out)
    assert len(out) == 3


def test_literals_become_natural_python_types(store):
    rows = store.query(f"SELECT ?v WHERE {{ GRAPH ?g {{ ?s <{EX}salary> ?v }} }}")
    values = sorted(r["v"] for r in rows)
    assert values == [88000, 95000, 102000]
    assert all(isinstance(v, int) for v in values)


def test_term_gives_the_lossless_form(store):
    rows = store.query(f"SELECT ?v WHERE {{ GRAPH ?g {{ ?s <{EX}salary> ?v }} }} LIMIT 1")
    assert "XMLSchema#integer" in rows[0].term("v")


def test_solution_access_by_name_position_and_dict(store):
    rows = store.query(f"SELECT ?s ?n WHERE {{ GRAPH ?g {{ ?s <{EX}name> ?n }} }} LIMIT 1")
    row = rows[0]
    assert row["s"] == row[0]
    assert row.variables == ["s", "n"]
    assert set(row.to_dict()) == {"s", "n"}
    assert len(row) == 2


def test_an_unbound_variable_is_none(store):
    rows = store.query(
        f"SELECT ?n ?missing WHERE {{ GRAPH ?g {{ ?s <{EX}name> ?n }} "
        f"OPTIONAL {{ ?s <{EX}nothing> ?missing }} }} LIMIT 1"
    )
    assert rows[0]["missing"] is None


def test_an_unknown_variable_raises(store):
    rows = store.query(f"SELECT ?n WHERE {{ GRAPH ?g {{ ?s <{EX}name> ?n }} }} LIMIT 1")
    with pytest.raises(KeyError):
        _ = rows[0]["nope"]


def test_a_syntax_error_is_its_own_exception(store):
    with pytest.raises(holosdb.SyntaxError):
        store.query("SELECT nonsense")


# --------------------------------------------------------------------------- policy


def test_a_denied_predicate_disappears(store):
    q = f"SELECT ?v WHERE {{ GRAPH ?g {{ ?s <{EX}salary> ?v }} }}"
    assert len(store.query(q)) == 3
    policy = Policy().deny_predicate(f"{EX}salary")
    assert store.query(q, policy=policy) == []


def test_except_role_exempts_the_role(store):
    # Deny beats allow at equal specificity, so "everyone except HR" is only expressible
    # through negation in the principal match. This is the test that it works.
    q = f"SELECT ?v WHERE {{ GRAPH ?g {{ ?s <{EX}salary> ?v }} }}"
    policy = Policy().deny_predicate(f"{EX}salary", except_role="hr")
    assert store.query(q, policy=policy, principal=Principal.anonymous()) == []
    assert len(store.query(q, policy=policy, principal=Principal(roles=["hr"]))) == 3


def test_clearance_gates_a_labelled_graph(store):
    q = f"SELECT ?n WHERE {{ GRAPH <{EX}reviews> {{ ?s <{EX}reviewNote> ?n }} }}"
    policy = Policy().label_graph(f"{EX}reviews", 3)
    assert store.query(q, policy=policy) == []
    assert len(store.query(q, policy=policy, principal=Principal(clearance=3))) == 1
    # Clearance 2 does not dominate a level-3 label.
    assert store.query(q, policy=policy, principal=Principal(clearance=2)) == []


def test_count_cannot_leak_hidden_rows(store):
    # The property that makes scan-level enforcement worth having: an aggregate sees the
    # sub-dataset, not the whole one with a filter on top.
    policy = Policy().deny_predicate(f"{EX}salary")
    rows = store.query(
        f"SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH ?g {{ ?s <{EX}salary> ?o }} }}",
        policy=policy,
    )
    assert rows[0]["n"] == 0


def test_filter_not_exists_cannot_probe_for_hidden_rows(store):
    # Without scan-level enforcement this is the classic leak: NOT EXISTS over a hidden
    # predicate would report "absent" for rows that are merely invisible — which is the
    # correct answer here, and the point is that it is *consistently* correct.
    policy = Policy().deny_predicate(f"{EX}salary")
    rows = store.query(
        f"SELECT ?n WHERE {{ GRAPH ?g {{ ?s <{EX}name> ?n }} "
        f"FILTER NOT EXISTS {{ ?s <{EX}salary> ?v }} }}",
        policy=policy,
    )
    assert len(rows) == 3


def test_deny_all_hides_everything_until_granted(store):
    q = f"SELECT ?n WHERE {{ GRAPH ?g {{ ?s <{EX}name> ?n }} }}"
    assert store.query(q, policy=Policy.deny_all()) == []
    granted = Policy.deny_all().allow_graph(f"{EX}public")
    assert len(store.query(q, policy=granted)) == 3


# --------------------------------------------------------------------------- writing


def test_add_a_quad():
    s = Store()
    assert s.add(f"{EX}a", f"{EX}p", "hello") is True
    assert s.add(f"{EX}a", f"{EX}p", "hello") is False  # idempotent
    assert len(s) == 1
    assert s.query("SELECT ?o WHERE { ?s ?p ?o }")[0]["o"] == "hello"


def test_add_typed_objects():
    s = Store()
    s.add(f"{EX}a", f"{EX}int", 42)
    s.add(f"{EX}a", f"{EX}float", 1.5)
    s.add(f"{EX}a", f"{EX}bool", True)
    s.add(f"{EX}a", f"{EX}iri", f"<{EX}target>")
    rows = {r["p"]: r["o"] for r in s.query("SELECT ?p ?o WHERE { ?s ?p ?o }")}
    assert rows[f"{EX}int"] == 42
    assert rows[f"{EX}float"] == 1.5
    assert rows[f"{EX}bool"] is True
    assert rows[f"{EX}iri"] == f"{EX}target"


def test_insert_data():
    s = Store()
    out = s.update(f"INSERT DATA {{ <{EX}a> <{EX}p> <{EX}b> }}")
    assert out["inserted"] == 1
    assert len(s) == 1


def test_delete_insert_rewrites():
    s = Store()
    s.update(f"INSERT DATA {{ <{EX}a> <{EX}status> <{EX}draft> }}")
    out = s.update(
        f"DELETE {{ ?s <{EX}status> <{EX}draft> }} "
        f"INSERT {{ ?s <{EX}status> <{EX}live> }} "
        f"WHERE  {{ ?s <{EX}status> <{EX}draft> }}"
    )
    assert (out["deleted"], out["inserted"]) == (1, 1)
    assert s.query(f"ASK {{ ?s <{EX}status> <{EX}live> }}") is True


def test_an_update_is_all_or_nothing():
    # The second operation cannot succeed, so the first must not survive either.
    s = Store()
    s.update(f"INSERT DATA {{ <{EX}keep> <{EX}p> <{EX}b> }}")
    with pytest.raises(holosdb.HolosError):
        s.update(
            f"INSERT DATA {{ <{EX}new> <{EX}p> <{EX}x> }} ; DROP GRAPH <{EX}missing>"
        )
    assert len(s) == 1, "the rolled-back insert must be gone, the earlier quad kept"


def test_graph_operations():
    s = Store()
    assert s.update(f"CREATE GRAPH <{EX}g>")["graphsCreated"] == 1
    s.update(f"INSERT DATA {{ GRAPH <{EX}g> {{ <{EX}a> <{EX}p> <{EX}b> }} }}")
    assert len(s) == 1
    assert s.update(f"DROP GRAPH <{EX}g>")["graphsDropped"] == 1
    assert len(s) == 0


def test_a_denied_write_is_refused_and_rolled_back():
    s = Store()
    policy = Policy().deny_predicate(f"{EX}secret")
    # deny_predicate covers reads; a write-denying rule needs the same scope on WRITE,
    # which the fluent API expresses through deny_all plus a grant.
    with pytest.raises(holosdb.PolicyError):
        s.update(
            f"INSERT DATA {{ <{EX}a> <{EX}p> <{EX}b> }}",
            policy=Policy.deny_all(),
        )
    assert len(s) == 0


def test_an_update_cannot_delete_what_the_principal_cannot_read():
    s = Store()
    s.update(f"INSERT DATA {{ <{EX}a> <{EX}salary> 100 . <{EX}a> <{EX}name> 'A' }}")
    policy = Policy().deny_predicate(f"{EX}salary")
    out = s.update("DELETE WHERE { ?s ?p ?o }", policy=policy)
    assert out["deleted"] == 1
    assert len(s) == 1, "the unreadable quad survives"


def test_an_update_syntax_error_is_reported():
    with pytest.raises(holosdb.SyntaxError):
        Store().update("INSERT DATA { not sparql")


# --------------------------------------------------------------------------- compression


def test_a_gzipped_ntriples_file_loads(tmp_path):
    import gzip

    path = tmp_path / "data.nt.gz"
    with gzip.open(path, "wt") as f:
        f.write(f"<{EX}a> <{EX}p> <{EX}o> .\n<{EX}b> <{EX}p> <{EX}o> .\n")
    s = Store()
    assert s.load(str(path)) == 2
    assert len(s) == 2


def test_a_gzipped_nquads_file_keeps_its_graphs(tmp_path):
    import gzip

    path = tmp_path / "data.nq.gz"
    with gzip.open(path, "wt") as f:
        f.write(f"<{EX}a> <{EX}p> <{EX}o> <{EX}g1> .\n")
        f.write(f"<{EX}b> <{EX}p> <{EX}o> <{EX}g2> .\n")
    s = Store()
    s.load(str(path))
    # Inferring N-Triples from the `.gz` would have flattened both graphs into one.
    assert len(s.named_graphs()) == 2


def test_concatenated_gzip_members_all_load(tmp_path):
    # A single-member decoder reads the first chunk and reports success, which is how a
    # concatenated dump loses most of itself without anyone noticing.
    import gzip
    import io as _io

    path = tmp_path / "multi.nt.gz"
    with open(path, "wb") as out:
        for i in range(4):
            buf = _io.BytesIO()
            with gzip.GzipFile(fileobj=buf, mode="wb") as g:
                g.write(f"<{EX}s{i}> <{EX}p> <{EX}o> .\n".encode())
            out.write(buf.getvalue())
    s = Store()
    assert s.load(str(path)) == 4


# --------------------------------------------------------------------------- geospatial


def test_geosparql_functions_include_the_ones_this_project_adds():
    # Deliberately not a count. This asserted `len(fns) == 45`, which went stale the moment
    # the set operations were wrapped and again when geof:distance was replaced -- and it
    # went stale silently, because nothing runs the wheel tests until a release. A count is
    # a test of arithmetic; what matters is that the functions are actually there.
    fns = set(holosdb.geosparql_functions())
    geof = "http://www.opengis.net/def/function/geosparql/"

    # Added by this project because spargeo does not carry them.
    for name in ["buffer", "boundary"]:
        assert geof + name in fns, f"geof:{name} is missing"

    # Replaced by this project, so they must still be registered exactly once each.
    for name in ["union", "intersection", "difference", "symDifference", "distance"]:
        assert geof + name in fns, f"geof:{name} is missing"

    # And the ones that come from spargeo, so a wiring mistake that dropped the whole
    # upstream set would be caught rather than looking like a shorter list.
    for name in ["sfWithin", "sfIntersects", "envelope", "asWKT"]:
        assert geof + name in fns, f"geof:{name} is missing"

    assert len(fns) == len(holosdb.geosparql_functions()), "the list contains duplicates"


def test_buffer_and_boundary_evaluate():
    s = Store()
    rows = s.query(
        """
        PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
        PREFIX geo:  <http://www.opengis.net/ont/geosparql#>
        PREFIX uom:  <http://www.opengis.net/def/uom/OGC/1.0/>
        SELECT ?area ?boundary WHERE {
          BIND(geof:area(geof:buffer("POINT(0 51.5)"^^geo:wktLiteral, 1000, uom:metre),
                         uom:square_metre) AS ?area)
          BIND(geof:boundary("POLYGON((0 0,0 1,1 1,1 0,0 0))"^^geo:wktLiteral) AS ?boundary)
        }
        """
    )
    # pi * 1000^2 = 3_141_593; the polygon approximating the circle lands just above.
    assert 3_100_000 < rows[0]["area"] < 3_200_000
    assert "MULTILINESTRING" in rows[0]["boundary"]


# --------------------------------------------------------------------------- shacl


def test_validate_reports_conformance(tmp_path):
    data = tmp_path / "d.ttl"
    data.write_text(
        f"@prefix ex: <{EX}> .\nex:a a ex:Person ; ex:name 'A' .\nex:b a ex:Person .\n"
    )
    shapes = tmp_path / "s.ttl"
    shapes.write_text(
        f"""@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <{EX}> .
ex:PersonShape a sh:NodeShape ;
  sh:targetClass ex:Person ;
  sh:property [ sh:path ex:name ; sh:minCount 1 ] .
"""
    )
    s = Store()
    s.load(str(data))
    report = s.validate(str(shapes))
    assert report["conforms"] is False
    assert report["violations"] == 1  # ex:b has no name


# --------------------------------------------------------------------------- persistence


@pytest.mark.skipif(not holosdb.has_rocksdb(), reason="built without persistence")
def test_a_persistent_store_survives_reopening():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "db")
        with Store(path) as s:
            s.add(f"{EX}a", f"{EX}p", "kept")
            assert s.is_persistent
        with Store(path) as again:
            assert len(again) == 1
            assert again.query("SELECT ?o WHERE { ?s ?p ?o }")[0]["o"] == "kept"


@pytest.mark.skipif(not holosdb.has_rocksdb(), reason="built without persistence")
def test_one_process_at_a_time_holds_a_store():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "db")
        with Store(path):
            # The exclusive RocksDB lock is what stops a second reader corrupting things.
            with pytest.raises(OSError):
                Store(path)
        # ...and releasing it is what makes the directory usable again.
        Store(path).close()


def test_close_is_idempotent_and_the_store_then_refuses_work():
    s = Store()
    s.add(f"{EX}a", f"{EX}p", "x")
    assert not s.is_closed
    s.close()
    s.close()  # closing twice is fine
    assert s.is_closed
    assert repr(s) == "Store(closed)"
    with pytest.raises(holosdb.HolosError):
        len(s)
    with pytest.raises(holosdb.HolosError):
        s.query("SELECT * WHERE { ?s ?p ?o }")


def test_the_context_manager_does_not_swallow_exceptions():
    with pytest.raises(ValueError):
        with Store() as s:
            s.add(f"{EX}a", f"{EX}p", "x")
            raise ValueError("boom")
