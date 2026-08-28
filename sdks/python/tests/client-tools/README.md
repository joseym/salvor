# Client-tool declarations for the Python LangChain middleware suite

These are what an operator writes and loads with `salvor serve --client-tool
<FILE>`. The middleware never sends an effect class, a schema or an idempotency
key: it sends a tool's name and the arguments the model produced, and everything
else about the call comes from the declaration the server was started with. A
tool with no declaration here is refused by name, which is what
`tests/test_langchain.py` proves in its undeclared-tool case.

They are byte-for-byte the declarations `sdks/typescript/test/client-tools/`
loads, apart from the path in a comment, because the two suites drive the same
two tools against the same server and an operator's declaration is not a
language's to change.

`notify-shipper.toml` declares `effect = "idempotent"` with `trust_completion = true`:
the one tool that tests what happens when an idempotent body raises. An intent
left without a completion is performed again on the next invoke, under the key
the recorded intent already fixed, so the provider sees one notice however many
times the call is attempted. See the effect-split cases in `tests/test_langchain.py`.
