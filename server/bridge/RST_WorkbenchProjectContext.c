#ifdef WORKBENCH
class RST_WorkbenchProjectContextRequest : JsonApiStruct
{
	void RST_WorkbenchProjectContextRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchProjectContextResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string loadedAddons;

	void RST_WorkbenchProjectContextResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchProjectContext : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchProjectContextRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchProjectContextResponse response = new RST_WorkbenchProjectContextResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		array<string> addonGuids = new array<string>();
		GameProject.GetLoadedAddons(addonGuids);
		foreach (string addonGuid : addonGuids)
		{
			string addonId = GameProject.GetAddonID(addonGuid);
			if (addonId != string.Empty)
			{
				if (response.loadedAddons != string.Empty)
					response.loadedAddons += ";";
				response.loadedAddons += addonId;
			}
		}
		return response;
	}
}
#endif
