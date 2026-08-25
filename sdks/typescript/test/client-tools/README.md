# Client-tool declarations for the LangChain middleware suite

These are what an operator writes and loads with `salvor serve --client-tool
<FILE>`. The middleware never sends an effect class, a schema or an idempotency
key: it sends a tool's name and the arguments the model produced, and everything
else about the call comes from the declaration the server was started with. A
tool with no declaration here is refused by name, which is what
`test/langchain.test.ts` proves in its last case.
