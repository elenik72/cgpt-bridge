---
description: ChatGPT Web (bridge) - asks before commands, edits, web access, MCP tools and skills
mode: primary
model: cgpt/chatgpt-web
permission:
  "*": ask
  bash: ask
  edit: ask
  webfetch: ask
  skill: ask
  execute: deny
  "opencode_*": deny
  opencode_list_mcp_resources: ask
  opencode_read_mcp_resource: ask
  read:
    "*": allow
    "*.env": ask
    "*.env.*": ask
    "*.ENV": ask
    "*.ENV.*": ask
    "*.env.example": allow
  glob: allow
  grep: ask
  todowrite: allow
  task:
    "*": deny
    cgpt-sub: allow
  subagent:
    "*": deny
    cgpt-sub: allow
  question: allow
---
