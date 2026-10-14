param([string]$SmokePath,[string]$PythonPath,[string]$FakePath,[string]$Case)
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'
& $SmokePath -Command $PythonPath -ArgumentList @('-u',$FakePath) -EnvironmentVariables @{MCP_SMOKE_CASE=$Case;AGENT_BUS_STARTUP_ENABLED='false'} -TimeoutSeconds 3
