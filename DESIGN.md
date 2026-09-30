# Design

- Harper transport boundary: the private `client` module owns HTTP, authentication, endpoint validation, limits, and Harper errors; JSON-RPC handlers only adapt request and response shapes, and tests exercise the boundary through the public dispatch entry point.
