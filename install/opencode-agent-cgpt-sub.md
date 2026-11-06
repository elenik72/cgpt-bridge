---
description: ChatGPT Web (bridge) subagent - asks before commands, edits, web access, MCP tools and skills
mode: subagent
model: cgpt/chatgpt-web
permission:
  "*": ask
  bash: ask
  shell: ask
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
  task: deny
  subagent: deny
  question: allow
---
