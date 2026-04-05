function widget:GetInfo()
	return {
		name    = "ZKScraper Replay Snapshot",
		desc    = "Captures replay snapshots and events for offline parsing",
		author  = "Codex",
		date    = "2026-03-29",
		license = "GNU GPL v2 or later",
		layer   = -1000,
		enabled = true,
	}
end

local spEcho = Spring.Echo
local spGetAllUnits = Spring.GetAllUnits
local spGetAllyTeamList = Spring.GetAllyTeamList
local spGetConfigInt = Spring.GetConfigInt
local spGetConfigString = Spring.GetConfigString
local spGetGameFrame = Spring.GetGameFrame
local spGetGameSpeed = Spring.GetGameSpeed
local spGetTimer = Spring.GetTimer
local spDiffTimers = Spring.DiffTimers
local spGetGaiaTeamID = Spring.GetGaiaTeamID
local spGetMyPlayerID = Spring.GetMyPlayerID
local spGetPlayerInfo = Spring.GetPlayerInfo
local spGetPlayerList = Spring.GetPlayerList
local spGetTeamInfo = Spring.GetTeamInfo
local spGetTeamList = Spring.GetTeamList
local spGetTeamResources = Spring.GetTeamResources
local spGetUnitDefID = Spring.GetUnitDefID
local spGetUnitExperience = Spring.GetUnitExperience
local spGetUnitHealth = Spring.GetUnitHealth
local spGetUnitHeading = Spring.GetUnitHeading
local spGetUnitLosState = Spring.GetUnitLosState
local spGetUnitPosition = Spring.GetUnitPosition
local spGetUnitTeam = Spring.GetUnitTeam
local spIsReplay = Spring.IsReplay
local spSendCommands = Spring.SendCommands

local metaFile
local globalSnapshotFile
local allyTeamSnapshotFile
local economySnapshotFile
local eventFile
local captureDir
local snapshotFrames = 120
local replaySpeed = 1000
local didQuit = false
local replayControlsDisabled = false
local inConsoleMsgHook = false
local pauseRecoveryPending = false
local pauseRecoveryStartedAt = nil
local allyTeamIDs = {}
local pauseRecoveryDelay = 0.1
local forceReplaySpeed

local function jsonEscape(str)
	str = tostring(str or "")
	str = str:gsub("\\", "\\\\")
	str = str:gsub("\"", "\\\"")
	str = str:gsub("\r", "\\r")
	str = str:gsub("\n", "\\n")
	str = str:gsub("\t", "\\t")
	return str
end

local function jsonString(str)
	return "\"" .. jsonEscape(str) .. "\""
end

local function jsonNumber(num)
	if not num or num ~= num or num == math.huge or num == -math.huge then
		return "0"
	end
	return tostring(num)
end

local function jsonBool(value)
	return value and "true" or "false"
end

local function arrayToJson(parts)
	return "[" .. table.concat(parts, ",") .. "]"
end

local function objectToJson(parts)
	return "{" .. table.concat(parts, ",") .. "}"
end

local function getGameSeconds(frame)
	return frame / 30
end

local function isSpectator()
	local playerID = spGetMyPlayerID()
	local _, _, spec = spGetPlayerInfo(playerID, false)
	return spec
end

local function writeLine(fileHandle, line)
	fileHandle:write(line)
	fileHandle:write("\n")
	fileHandle:flush()
end

local function closeFiles()
	if metaFile then
		metaFile:flush()
		metaFile:close()
		metaFile = nil
	end
	if globalSnapshotFile then
		globalSnapshotFile:flush()
		globalSnapshotFile:close()
		globalSnapshotFile = nil
	end
	if allyTeamSnapshotFile then
		allyTeamSnapshotFile:flush()
		allyTeamSnapshotFile:close()
		allyTeamSnapshotFile = nil
	end
	if economySnapshotFile then
		economySnapshotFile:flush()
		economySnapshotFile:close()
		economySnapshotFile = nil
	end
	if eventFile then
		eventFile:flush()
		eventFile:close()
		eventFile = nil
	end
end

local function playerListJson()
	local playerParts = {}
	for _, playerID in ipairs(spGetPlayerList()) do
		local name, active, spectator, teamID = spGetPlayerInfo(playerID, false)
		playerParts[#playerParts + 1] = objectToJson({
			"\"player_id\":" .. jsonNumber(playerID),
			"\"name\":" .. jsonString(name or ""),
			"\"active\":" .. jsonBool(active),
			"\"spectator\":" .. jsonBool(spectator),
			"\"team_id\":" .. jsonNumber(teamID or -1),
		})
	end
	return arrayToJson(playerParts)
end

local function writeMeta()
	local allyTeamParts = {}
	for _, allyTeamID in ipairs(allyTeamIDs) do
		allyTeamParts[#allyTeamParts + 1] = jsonNumber(allyTeamID)
	end
	local metaJson = objectToJson({
		"\"map_size\":"
			.. objectToJson({
				"\"x\":" .. jsonNumber(Game.mapSizeX),
				"\"z\":" .. jsonNumber(Game.mapSizeZ),
			}),
		"\"players\":" .. playerListJson(),
		"\"allyteams\":" .. arrayToJson(allyTeamParts),
	})

	metaFile:write(metaJson)
	metaFile:flush()
end

local function unitSnapshotJson(unitID)
	local unitDefID = spGetUnitDefID(unitID)
	if not unitDefID or not UnitDefs[unitDefID] then
		return nil
	end

	local teamID = spGetUnitTeam(unitID) or -1
	local allyTeamID = select(6, spGetTeamInfo(teamID, false)) or -1
	local posX, posY, posZ = spGetUnitPosition(unitID)
	if not posX then
		return nil
	end

	local hp, maxHp, _, _, buildProgress = spGetUnitHealth(unitID)
	return objectToJson({
		"\"unit_id\":" .. jsonNumber(unitID),
		"\"unit_def_name\":" .. jsonString(UnitDefs[unitDefID].name),
		"\"team_id\":" .. jsonNumber(teamID),
		"\"allyteam_id\":" .. jsonNumber(allyTeamID),
		"\"x\":" .. jsonNumber(posX),
		"\"y\":" .. jsonNumber(posY),
		"\"z\":" .. jsonNumber(posZ),
		"\"hp\":" .. jsonNumber(hp or 0),
		"\"max_hp\":" .. jsonNumber(maxHp or 0),
		"\"build_progress\":" .. jsonNumber(buildProgress or 1),
		"\"heading\":" .. jsonNumber(spGetUnitHeading(unitID) or 0),
		"\"experience\":" .. jsonNumber(spGetUnitExperience(unitID) or 0),
	})
end

local function writeSnapshot(frame)
	local units = {}
	for _, unitID in ipairs(spGetAllUnits()) do
		local unitJson = unitSnapshotJson(unitID)
		if unitJson then
			units[#units + 1] = unitJson
		end
	end

	writeLine(globalSnapshotFile, objectToJson({
		"\"frame\":" .. jsonNumber(frame),
		"\"game_seconds\":" .. jsonNumber(getGameSeconds(frame)),
		"\"units\":" .. arrayToJson(units),
	}))
end

local function radarContactJson(unitID)
	local teamID = spGetUnitTeam(unitID) or -1
	local allyTeamID = select(6, spGetTeamInfo(teamID, false)) or -1
	local posX, posY, posZ = spGetUnitPosition(unitID)
	if not posX then
		return nil
	end

	return objectToJson({
		"\"unit_id\":" .. jsonNumber(unitID),
		"\"team_id\":" .. jsonNumber(teamID),
		"\"allyteam_id\":" .. jsonNumber(allyTeamID),
		"\"x\":" .. jsonNumber(posX),
		"\"y\":" .. jsonNumber(posY),
		"\"z\":" .. jsonNumber(posZ),
	})
end

local function writeAllyTeamSnapshots(frame)
	for _, allyTeamID in ipairs(allyTeamIDs) do
		local losUnits = {}
		local radarContacts = {}
		for _, unitID in ipairs(spGetAllUnits()) do
			local losState = spGetUnitLosState(unitID, allyTeamID) or {}
			if losState.los then
				local unitJson = unitSnapshotJson(unitID)
				if unitJson then
					losUnits[#losUnits + 1] = unitJson
				end
			elseif losState.radar then
				local radarJson = radarContactJson(unitID)
				if radarJson then
					radarContacts[#radarContacts + 1] = radarJson
				end
			end
		end

		writeLine(allyTeamSnapshotFile, objectToJson({
			"\"allyteam_id\":" .. jsonNumber(allyTeamID),
			"\"frame\":" .. jsonNumber(frame),
			"\"game_seconds\":" .. jsonNumber(getGameSeconds(frame)),
			"\"los_units\":" .. arrayToJson(losUnits),
			"\"radar_contacts\":" .. arrayToJson(radarContacts),
		}))
	end
end

local function economyJsonForTeam(teamID)
	local metalCurrent, metalStorage, metalPull, metalIncome, metalExpense, metalShare, metalSent, metalReceived =
		spGetTeamResources(teamID, "metal")
	local energyCurrent, energyStorage, energyPull, energyIncome, energyExpense, energyShare, energySent, energyReceived =
		spGetTeamResources(teamID, "energy")

	return objectToJson({
		"\"metal_income\":" .. jsonNumber(metalIncome or 0),
		"\"energy_income\":" .. jsonNumber(energyIncome or 0),
		"\"metal_stored\":" .. jsonNumber(metalCurrent or 0),
		"\"energy_stored\":" .. jsonNumber(energyCurrent or 0),
		"\"metal_storage\":" .. jsonNumber(metalStorage or 0),
		"\"energy_storage\":" .. jsonNumber(energyStorage or 0),
		"\"metal_pull\":" .. jsonNumber(metalPull or 0),
		"\"energy_pull\":" .. jsonNumber(energyPull or 0),
		"\"metal_expense\":" .. jsonNumber(metalExpense or 0),
		"\"energy_expense\":" .. jsonNumber(energyExpense or 0),
		"\"metal_share\":" .. jsonNumber(metalShare or 0),
		"\"energy_share\":" .. jsonNumber(energyShare or 0),
		"\"metal_sent\":" .. jsonNumber(metalSent or 0),
		"\"energy_sent\":" .. jsonNumber(energySent or 0),
		"\"metal_received\":" .. jsonNumber(metalReceived or 0),
		"\"energy_received\":" .. jsonNumber(energyReceived or 0),
	})
end

local function writeEconomySnapshots(frame)
	local seenTeams = {}
	for _, playerID in ipairs(spGetPlayerList()) do
		local _, active, spectator, teamID = spGetPlayerInfo(playerID, false)
		if not spectator and teamID and teamID >= 0 and not seenTeams[teamID] then
			seenTeams[teamID] = true
			local allyTeamID = select(6, spGetTeamInfo(teamID, false)) or -1
			writeLine(economySnapshotFile, objectToJson({
				"\"team_id\":" .. jsonNumber(teamID),
				"\"allyteam_id\":" .. jsonNumber(allyTeamID),
				"\"frame\":" .. jsonNumber(frame),
				"\"game_seconds\":" .. jsonNumber(getGameSeconds(frame)),
				"\"economy\":" .. economyJsonForTeam(teamID),
			}))
		end
	end
end

local function writeEvent(eventType, frame, payloadJson)
	payloadJson = payloadJson or "{}"
	writeLine(eventFile, objectToJson({
		"\"event_type\":" .. jsonString(eventType),
		"\"frame\":" .. jsonNumber(frame),
		"\"game_seconds\":" .. jsonNumber(getGameSeconds(frame)),
		"\"payload\":" .. payloadJson,
	}))
end

local function unitEventPayload(unitID, unitDefID, teamID)
	unitDefID = unitDefID or spGetUnitDefID(unitID)
	local unitName = (unitDefID and UnitDefs[unitDefID] and UnitDefs[unitDefID].name) or ""
	local posX, posY, posZ = spGetUnitPosition(unitID)
	return objectToJson({
		"\"unit_id\":" .. jsonNumber(unitID),
		"\"unit_def_id\":" .. jsonNumber(unitDefID or -1),
		"\"unit_def_name\":" .. jsonString(unitName),
		"\"team_id\":" .. jsonNumber(teamID or -1),
		"\"x\":" .. jsonNumber(posX or 0),
		"\"y\":" .. jsonNumber(posY or 0),
		"\"z\":" .. jsonNumber(posZ or 0),
	})
end

local function quitReplay()
	if didQuit then
		return
	end
	didQuit = true
	closeFiles()
	spSendCommands("quitforce")
end

local function isGamePaused()
	return select(3, spGetGameSpeed())
end

local function unpauseReplay()
	if not isGamePaused() then
		return
	end
	spSendCommands("pause 0")
end

local function requestPauseRecovery()
	pauseRecoveryPending = true
	pauseRecoveryStartedAt = spGetTimer()
end

local function clearPauseRecovery()
	pauseRecoveryPending = false
	pauseRecoveryStartedAt = nil
end

local function updatePauseRecovery()
	if not pauseRecoveryPending then
		return
	end
	if not isGamePaused() then
		clearPauseRecovery()
		return
	end
	if not pauseRecoveryStartedAt then
		pauseRecoveryStartedAt = spGetTimer()
		return
	end
	if spDiffTimers(spGetTimer(), pauseRecoveryStartedAt) < pauseRecoveryDelay then
		return
	end

	clearPauseRecovery()
	spEcho("<ZKScraper> Pause detected; resuming replay.")
	unpauseReplay()
	forceReplaySpeed()
end

local function disableReplayControls()
	if replayControlsDisabled then
		return
	end
	if not widgetHandler or not widgetHandler.knownWidgets then
		return
	end
	if not widgetHandler.knownWidgets["Replay control buttons"] then
		return
	end

	widgetHandler:DisableWidget("Replay control buttons")
	replayControlsDisabled = true
	spEcho("<ZKScraper> Disabled Replay control buttons.")
	unpauseReplay()
	spSendCommands("setminspeed " .. replaySpeed)
	spSendCommands("setmaxspeed " .. replaySpeed)
end

forceReplaySpeed = function()
	spSendCommands("setminspeed " .. replaySpeed)
	spSendCommands("setmaxspeed " .. replaySpeed)
end

function widget:Initialize()
	if not isSpectator() then
		spEcho("<ZKScraper> Not spectating. Widget removed.")
		widgetHandler:RemoveWidget()
		return
	end
	if not spIsReplay() then
		spEcho("<ZKScraper> Not a replay. Widget removed.")
		widgetHandler:RemoveWidget()
		return
	end

	captureDir = spGetConfigString("ZKHeadlessOutputDir", "")
	snapshotFrames = spGetConfigInt("ZKHeadlessSnapshotFrames", 120) or 120
	replaySpeed = spGetConfigInt("ZKHeadlessReplaySpeed", 1000) or 1000
	if captureDir == "" then
		spEcho("<ZKScraper> No configured output directory. Widget removed.")
		widgetHandler:RemoveWidget()
		return
	end
	Spring.CreateDir(captureDir)
	local gaiaAllyTeamID = select(6, spGetTeamInfo(spGetGaiaTeamID(), false))
	for _, allyTeamID in ipairs(spGetAllyTeamList()) do
		local teams = spGetTeamList(allyTeamID) or {}
		if allyTeamID ~= gaiaAllyTeamID and #teams > 0 then
			allyTeamIDs[#allyTeamIDs + 1] = allyTeamID
		end
	end

	local err
	metaFile, err = io.open(captureDir .. "/meta.json", "w+")
	if not metaFile then
		spEcho("<ZKScraper> Failed to open meta file: " .. tostring(err))
		widgetHandler:RemoveWidget()
		return
	end

	globalSnapshotFile, err = io.open(captureDir .. "/global_snapshots.jsonl", "w+")
	if not globalSnapshotFile then
		spEcho("<ZKScraper> Failed to open global snapshot file: " .. tostring(err))
		closeFiles()
		widgetHandler:RemoveWidget()
		return
	end

	allyTeamSnapshotFile, err = io.open(captureDir .. "/allyteam_snapshots.jsonl", "w+")
	if not allyTeamSnapshotFile then
		spEcho("<ZKScraper> Failed to open allyteam snapshot file: " .. tostring(err))
		closeFiles()
		widgetHandler:RemoveWidget()
		return
	end

	economySnapshotFile, err = io.open(captureDir .. "/economy_snapshots.jsonl", "w+")
	if not economySnapshotFile then
		spEcho("<ZKScraper> Failed to open economy snapshot file: " .. tostring(err))
		closeFiles()
		widgetHandler:RemoveWidget()
		return
	end

	eventFile, err = io.open(captureDir .. "/events.jsonl", "w+")
	if not eventFile then
		spEcho("<ZKScraper> Failed to open event file: " .. tostring(err))
		closeFiles()
		widgetHandler:RemoveWidget()
		return
	end

	writeMeta()
	writeEvent("capture_started", spGetGameFrame(), objectToJson({
		"\"battle_id\":" .. jsonNumber(spGetConfigInt("ZKHeadlessBattleId", -1)),
		"\"snapshot_frames\":" .. jsonNumber(snapshotFrames),
		"\"replay_speed\":" .. jsonNumber(replaySpeed),
	}))

	spSendCommands("forcestart")
	spSendCommands("skip 0")
	disableReplayControls()
	unpauseReplay()
	forceReplaySpeed()
end

function widget:Shutdown()
	closeFiles()
end

function widget:Update()
	disableReplayControls()
	updatePauseRecovery()
end

function widget:GamePaused(playerID, paused)
	if not paused then
		clearPauseRecovery()
		return
	end

	disableReplayControls()
	requestPauseRecovery()
end

function widget:GameFrame(frame)
	if frame % 300 == 0 then
		forceReplaySpeed()
	end
	if frame % snapshotFrames == 0 then
		writeSnapshot(frame)
		writeAllyTeamSnapshots(frame)
		writeEconomySnapshots(frame)
	end
end

function widget:AddConsoleMessage(msg)
	if inConsoleMsgHook then
		return
	end
	if msg.text ~= "Beginning demo playback" then
		return
	end
	inConsoleMsgHook = true
	unpauseReplay()
	forceReplaySpeed()
	clearPauseRecovery()
	inConsoleMsgHook = false
end

function widget:UnitFinished(unitID, unitDefID, unitTeam)
	writeEvent("unit_finished", spGetGameFrame(), unitEventPayload(unitID, unitDefID, unitTeam))
end

function widget:UnitDestroyed(unitID, unitDefID, unitTeam, attackerID, attackerDefID, attackerTeam)
	writeEvent("unit_destroyed", spGetGameFrame(), objectToJson({
		"\"unit\":"
			.. unitEventPayload(unitID, unitDefID, unitTeam),
		"\"attacker_id\":" .. jsonNumber(attackerID or -1),
		"\"attacker_def_id\":" .. jsonNumber(attackerDefID or -1),
		"\"attacker_team\":" .. jsonNumber(attackerTeam or -1),
	}))
end

function widget:UnitGiven(unitID, unitDefID, newTeamID, oldTeamID)
	writeEvent("unit_given", spGetGameFrame(), objectToJson({
		"\"unit\":"
			.. unitEventPayload(unitID, unitDefID, newTeamID),
		"\"old_team_id\":" .. jsonNumber(oldTeamID or -1),
		"\"new_team_id\":" .. jsonNumber(newTeamID or -1),
	}))
end

function widget:UnitTaken(unitID, unitDefID, oldTeamID, newTeamID)
	writeEvent("unit_taken", spGetGameFrame(), objectToJson({
		"\"unit\":"
			.. unitEventPayload(unitID, unitDefID, oldTeamID),
		"\"old_team_id\":" .. jsonNumber(oldTeamID or -1),
		"\"new_team_id\":" .. jsonNumber(newTeamID or -1),
	}))
end

function widget:PlayerChanged(playerID)
	local name, active, spectator, teamID = spGetPlayerInfo(playerID, false)
	writeEvent("player_changed", spGetGameFrame(), objectToJson({
		"\"player_id\":" .. jsonNumber(playerID),
		"\"name\":" .. jsonString(name or ""),
		"\"active\":" .. jsonBool(active),
		"\"spectator\":" .. jsonBool(spectator),
		"\"team_id\":" .. jsonNumber(teamID or -1),
	}))
end

function widget:PlayerRemoved(playerID, reason)
	writeEvent("player_removed", spGetGameFrame(), objectToJson({
		"\"player_id\":" .. jsonNumber(playerID),
		"\"reason\":" .. jsonString(reason or ""),
	}))
end

function widget:TeamDied(teamID)
	writeEvent("team_died", spGetGameFrame(), objectToJson({
		"\"team_id\":" .. jsonNumber(teamID),
	}))
end

function widget:GameOver(winners)
	local gaiaAllyTeamID = select(6, spGetTeamInfo(spGetGaiaTeamID(), false))
	local winnerParts = {}
	for _, allyTeamID in ipairs(winners or {}) do
		winnerParts[#winnerParts + 1] = jsonNumber(allyTeamID)
	end

	writeEvent("game_over", spGetGameFrame(), objectToJson({
		"\"winner_allyteams\":" .. arrayToJson(winnerParts),
		"\"draw\":" .. jsonBool((winners and winners[1] == gaiaAllyTeamID) or false),
	}))
	quitReplay()
end
