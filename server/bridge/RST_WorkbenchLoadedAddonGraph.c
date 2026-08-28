#ifdef WORKBENCH
class RST_WorkbenchLoadedAddonGraphRequest : JsonApiStruct
{
	void RST_WorkbenchLoadedAddonGraphRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchLoadedAddonGraphResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string currentProjectFile;
	string graphJson;

	void RST_WorkbenchLoadedAddonGraphResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchLoadedAddonGraph : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchLoadedAddonGraphRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchLoadedAddonGraphResponse response = new RST_WorkbenchLoadedAddonGraphResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.currentProjectFile = Workbench.GetCurrentGameProjectFile();
		array<string> addonGuids = new array<string>();
		GameProject.GetLoadedAddons(addonGuids);
		response.graphJson = "[";
		foreach (string addonGuid : addonGuids)
		{
			string addonId = GameProject.GetAddonID(addonGuid);
			string sourceRoot;
			Workbench.GetAbsolutePath("$" + addonId + ":", sourceRoot, false);
			if (!sourceRoot.IsEmpty() && sourceRoot.EndsWith("/"))
				sourceRoot = sourceRoot.Substring(0, sourceRoot.Length() - 1);
			if (response.graphJson != "[")
				response.graphJson += ",";
			response.graphJson += "{\"guid\":\"" + EscapeJson(addonGuid)
				+ "\",\"id\":\"" + EscapeJson(addonId)
				+ "\",\"title\":\"" + EscapeJson(GameProject.GetAddonTitle(addonGuid))
				+ "\",\"sourceRoot\":\"" + EscapeJson(sourceRoot) + "\"}";
		}
		response.graphJson += "]";
		return response;
	}

	protected string EscapeJson(string value)
	{
		value.Replace("\\", "\\\\");
		value.Replace("\"", "\\\"");
		value.Replace("\n", "\\n");
		value.Replace("\r", "\\r");
		value.Replace("\t", "\\t");
		return value;
	}
}
#endif
