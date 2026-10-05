# snmp — priv (runtime data)

## Intent
Runtime data directory (`priv/`) of the `snmp` application.
Ships example/default SNMP **agent** and **manager** configuration files under `conf/`, and hosts two git-ignored build-output directories (`mibs/` for compiled MIBs, `plt/` for Dialyzer PLTs).

## API Surface
- `conf/` — example configuration files, organized into `agent/` and `manager/` subdirs (see routing table). Installed to `$(RELEASE_PATH)/lib/snmp-$(VSN)/priv/conf/...` at release time.
- `mibs/` — destination for compiled `*.bin` MIB files produced by the `mibs/` build of the app (only `.gitignore` is tracked here). Git-ignored.
- `plt/` — destination for the app's Dialyzer PLT (`DIA_PLT = ./priv/plt/$(APPLICATION).plt`, set in `lib/snmp/Makefile`) and `*.dialyzer_analysis`. Git-ignored.

### Agent conf files (`conf/agent/`)
- `agent.conf` — agent address/identity; maps to `intAgent*` vars of INTERNAL-MIB plus `snmpEngineID`/`snmpEngineMaxMessageSize`.
- `community.conf` — community → VACM mapping (`snmpCommunityTable`, SNMP-COMMUNITY-MIB); rows `{CommunityIndex, CommunityName, SecurityName, ContextName, TransportTag}`.
- `context.conf` — contexts known to the agent (`vacmContextTable`, SNMP-VIEW-BASED-ACM-MIB); one context-name string per row (empty string = default context).
- `notify.conf` — notification parameters (`snmpNotifyTable`, SNMP-NOTIFICATION-MIB); rows `{Name, Tag, Type}`.
- `standard.conf` — STANDARD-MIB info (`sysDescr`, `sysObjectID`, `sysContact`, `sysLocation`, `sysServices`, `snmpEnableAuthenTraps`).
- `target_addr.conf` — target addresses (`snmpTargetAddrTable` / `snmpTargetAddrExtTable`); 10/11-tuples with optional `Domain`.
- `target_params.conf` — target parameters (`snmpTargetParamsTable`, SNMP-TARGET-MIB); rows `{Name, MPModel, SecurityModel, SecurityName, SecurityLevel}`.
- `usm.conf` — USM security parameters (`usmUserTable`, SNMP-USER-BASED-SM-MIB); 14-tuple rows.
- `vacm.conf` — MIB views (`vacmSecurityToGroup` / `vacmAccess` / `vacmViewTreeFamily` rows in SNMP-VIEW-BASED-ACM-MIB).

### Manager conf files (`conf/manager/`)
- `manager.conf` — general manager params: `{address, Port, engine_id, max_message_size}`.
- `agents.conf` — agents the manager handles (relevant parts of `snmpTargetAddrTable`/`snmpTargetParamsTable`); per-agent `{UserId, TargetName, Comm, Ip, Port, EngineID, Timeout, MaxMessageSize, Version, SecModel, SecName, SecLevel}`.
- `users.conf` — manager users: `{UserId, UserMod, UserData}` (UserMod implements the `snmpm_user` behaviour).
- `usm.conf` — USM user table data for the manager (`{EngineID, UserName, AuthP, AuthKey, PrivP, PrivKey}` etc.).

## Constraints
- Conf files are installed via `files.mk` in each subdir: `conf/agent/files.mk` (`CONF_FILES = agent.conf community.conf context.conf notify.conf standard.conf target_addr.conf target_params.conf usm.conf vacm.conf`) and `conf/manager/files.mk` (`CONF_FILES = manager.conf agents.conf users.conf`).
- A new conf file must be added to the relevant `files.mk` (`CONF_FILES`) to be installed.
- `make` in `conf/` recurses into `agent`/`manager` (driven by `conf/subdirs.mk`: `SUB_DIRECTORIES = agent manager`).
- Do not hand-edit `mibs/*.bin` or `plt/*.plt` — these are regenerated build outputs.
- Conf files are Erlang terms (one datum per dot-terminated line); comments use `%%`.

## Known Issues
- `conf/manager/usm.conf` is tracked in git but is NOT listed in `conf/manager/files.mk` `CONF_FILES`, so it is not installed with the manager conf set (unlike its agent counterpart). Treat `files.mk` as the authoritative installed set.

## Notes for Agents
- `conf/agent/*.conf` examples carry a `snmp_config (vX.Y)` generation banner from historical tooling; content is illustrative, not authoritative system data.
- Only `conf/` has a Makefile tree; `priv/`, `mibs/` and `plt/` contain only `.gitignore` placeholders (plus generated outputs).

## Routing Table
- `conf/` → Example/default SNMP configuration files (agent + manager) and their install makefiles
  - `conf/agent/` → Agent config examples (agent, community, context, notify, standard, target_addr, target_params, usm, vacm)
  - `conf/manager/` → Manager config examples (manager.conf, agents.conf, users.conf, usm.conf)
- `mibs/` → Compiled MIB `*.bin` build output (git-ignored; only `.gitignore` tracked)
- `plt/` → Dialyzer PLT / analysis build output (git-ignored; only `.gitignore` tracked)

## See Also
- `../mibs/` (source MIBs) and `../Makefile` (`DIA_PLT` path) — parent app dir.
- `../src/` — the SNMP agent (`snmpa*`) and manager (`snmpm*`) modules that consume these conf files.
