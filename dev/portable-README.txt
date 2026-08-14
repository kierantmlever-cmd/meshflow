MeshFlow — portable build

Run meshflow.exe. No installation, no admin rights.

Everything it writes — config.toml, meshflow.db, logs — stays in the meshflow-data folder next to
the executable, so the whole directory can live on a USB stick and leaves nothing behind on the
machine it runs on. Delete meshflow-data and the same executable reverts to the usual Windows
locations under %APPDATA%. Setting MESHFLOW_HOME overrides both.

API keys are the exception: they go to the Windows Credential Manager, which is per-user and
per-machine. A portable copy on another machine will ask for the key again — a credential that
travelled around in a folder on a stick would not be one worth having.

First run: open Settings, add a provider and paste its API key.
