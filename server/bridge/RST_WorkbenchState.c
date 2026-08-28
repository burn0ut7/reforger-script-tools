#ifdef WORKBENCH
class RST_WorkbenchStateRequest : JsonApiStruct
{
	bool executeReloadAction;
	bool executeSaveAllAction;
	bool executeSaveWorldAction;

	void RST_WorkbenchStateRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchStateResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string mode;
	string activeWorldPath;
	bool worldEditorActive;
	bool worldEditorModulePresent;
	bool worldEditorApiAvailable;
	string playSession;
	string loadedAddons;
	int currentSubScene;
	int currentEntityLayerId;
	string activeSubsceneLayer;
	bool reloadActionAccepted;
	string reloadActionPath;
	bool saveAllActionAccepted;
	string saveAllActionPath;
	bool worldSaveActionAccepted;
	string worldSaveActionPath;
	string worldSaveStatus;

	void RST_WorkbenchStateResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchState : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchStateRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchStateRequest typedRequest = RST_WorkbenchStateRequest.Cast(request);
		RST_WorkbenchStateResponse response = new RST_WorkbenchStateResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.mode = "workbench";
		response.playSession = "unavailable";
		response.reloadActionPath = "Plugins/Settings/Reload WB Scripts";
		response.saveAllActionPath = "File/Save All";
		response.worldSaveActionPath = "WorldEditor.Save";
		response.worldSaveStatus = "skipped-no-open-world";
		WorldEditor worldEditor = Workbench.GetModule(WorldEditor);
		if (typedRequest && typedRequest.executeReloadAction)
		{
			ResourceManager resourceManager = Workbench.GetModule(ResourceManager);
			if (resourceManager)
			{
				array<string> menuPath = {
					"Plugins", "Settings", "Reload WB Scripts"
				};
				response.reloadActionAccepted = resourceManager.ExecuteAction(menuPath, true);
			}
		}
		if (typedRequest && typedRequest.executeSaveAllAction)
		{
			ResourceManager resourceManager = Workbench.GetModule(ResourceManager);
			if (resourceManager)
			{
				array<string> menuPath = {
					"File", "Save All"
				};
				response.saveAllActionAccepted = resourceManager.ExecuteAction(menuPath, true);
			}
			if (worldEditor)
			{
				WorldEditorAPI worldEditorApi = worldEditor.GetApi();
				string worldPath;
				if (worldEditorApi)
					worldEditorApi.GetWorldPath(worldPath);
				if (!worldPath.IsEmpty())
				{
					response.worldSaveActionAccepted = worldEditor.Save();
					if (response.worldSaveActionAccepted)
						response.worldSaveStatus = "saved";
					else
						response.worldSaveStatus = "rejected";
				}
			}
		}
		if (typedRequest && typedRequest.executeSaveWorldAction && worldEditor)
		{
			WorldEditorAPI worldEditorApi = worldEditor.GetApi();
			string worldPath;
			if (worldEditorApi)
				worldEditorApi.GetWorldPath(worldPath);
			if (!worldPath.IsEmpty())
			{
				response.worldSaveActionAccepted = worldEditor.Save();
				if (response.worldSaveActionAccepted)
					response.worldSaveStatus = "saved";
				else
					response.worldSaveStatus = "rejected";
			}
		}
		if (worldEditor)
		{
			response.worldEditorModulePresent = true;
			WorldEditorAPI worldEditorApi = worldEditor.GetApi();
			if (worldEditorApi)
			{
				worldEditorApi.GetWorldPath(response.activeWorldPath);
				response.mode = "world-editor";
				response.worldEditorActive = true;
				response.worldEditorApiAvailable = true;
				response.playSession = "unknown";
				response.currentSubScene = worldEditorApi.GetCurrentSubScene();
				response.currentEntityLayerId = worldEditorApi.GetCurrentEntityLayerId();
				response.activeSubsceneLayer = worldEditorApi.GetActiveSubsceneLayer(response.currentSubScene);
			}
			else
			{
				response.playSession = "likely-running";
			}
		}
		array<string> addonGuids = {
		};
		GameProject.GetLoadedAddons(addonGuids);
		for (int index = 0; index < addonGuids.Count(); index++)
		{
			if (index > 0)
				response.loadedAddons += ";";
			response.loadedAddons += GameProject.GetAddonID(addonGuids[index]);
		}
		return response;
	}
}
#endif
