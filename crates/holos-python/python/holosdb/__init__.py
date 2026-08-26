"""HOLOS — an RDF 1.2 triplestore with SPARQL 1.2 and policy enforced at the index scan.

The surface mirrors ``pyoxigraph`` where the two overlap, so that anyone already working in
this ecosystem does not have to learn a second vocabulary for the same ideas. What it adds
is :class:`Policy` and :class:`Principal`: a query can be asked *as somebody*, and the
answer is the one that principal is entitled to.

    >>> from holosdb import Store, Principal, Policy
    >>> store = Store()
    >>> store.load("examples/hr.trig")
    21
    >>> for row in store.query("SELECT ?s WHERE { GRAPH ?g { ?s ?p ?o } } LIMIT 1"):
    ...     print(row["s"])

Policy is applied at the index scan rather than by rewriting the query, which buys a
property worth stating exactly:

    the answer to Q equals the answer Q would have over the sub-dataset the principal
    may see.

That holds for *every* query shape without anyone enumerating them — a ``COUNT`` cannot
leak the existence of hidden rows, and a ``FILTER NOT EXISTS`` cannot probe for them.
"""

from ._holosdb import (  # noqa: F401
    HolosError,
    PolicyError,
    Policy,
    Principal,
    QuerySolution,
    Store,
    SyntaxError,
    __version__,
    geosparql_functions,
    has_rocksdb,
)

__all__ = [
    "Store",
    "Principal",
    "Policy",
    "QuerySolution",
    "HolosError",
    "SyntaxError",
    "PolicyError",
    "has_rocksdb",
    "geosparql_functions",
    "__version__",
]
