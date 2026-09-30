# Design

- Harper transport boundary: the private `client` module owns HTTP, authentication, endpoint validation, limits, and Harper errors; JSON-RPC handlers only adapt request and response shapes, and tests exercise the boundary through the public dispatch entry point.
- Each JSON-RPC request creates one lightweight Harper client facade with one absolute deadline. The shared asynchronous HTTP client and its single-worker runtime stay alive for connection pooling and idle-socket maintenance; the outer deadline covers request sending and the complete response body.
- Ordinary operations have a 105-second budget so a valid Harper request is not cut off by the former 50-second transport timeout. Connectivity probes use 15 seconds so `ping` and `test_connection` cannot stall the serial stdio queue for the full operation budget.
- Mutation fidelity: an advertised Tabularis mutation succeeds only when a native Harper operation or supported Harper SQL statement proves that exact mutation; unsupported schema/index semantics fail explicitly, and update/delete verify the table's real primary key before writing.
- SQL writes must name `database.table`; bare INSERT, UPDATE, and DELETE targets are rejected so Harper cannot resolve a mutation into a different database than the user intended.
- Execution is intentionally serial over stdio. The short-lived primary-key cache is thread-local to that model, and schema invalidation must be redesigned if dispatch is ever parallelized. Long Harper schema operations can delay unrelated requests on the same plugin process.
