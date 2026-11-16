Set WshShell = CreateObject("WScript.Shell")
WshShell.Run Chr(34) + "C:\Program Files\Git\bin\bash.exe" + Chr(34) + " -lc /e/ChatGPT/kuzu_memory/agent-memory-rs/scripts/start_memory_proxy.sh --daemon", 0, False
