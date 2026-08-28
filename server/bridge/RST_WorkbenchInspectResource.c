#ifdef WORKBENCH
class RST_WorkbenchInspectResourceRequest : JsonApiStruct
{
	string resourceName;

	void RST_WorkbenchInspectResourceRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchInspectResourceResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	bool found;
	string status;
	string resourceName;
	string className;
	string sourceAddons;
	bool sourceAddonsTruncated;

	void RST_WorkbenchInspectResourceResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchInspectResource : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchInspectResourceRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchInspectResourceRequest typedRequest = RST_WorkbenchInspectResourceRequest.Cast(request);
		RST_WorkbenchInspectResourceResponse response = new RST_WorkbenchInspectResourceResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		ResourceManager resourceManager = Workbench.GetModule(ResourceManager);
		if (!resourceManager)
		{
			response.status = "resource-manager-unavailable";
			return response;
		}
		ResourceName resourceName = typedRequest.resourceName;
		MetaFile meta = resourceManager.GetMetaFile(resourceName.GetPath());
		if (!meta)
		{
			response.status = "resource-not-found";
			return response;
		}
		ref BaseContainerList configurations = meta.GetObjectArray("Configurations");
		if (!configurations || configurations.Count() == 0)
		{
			response.status = "resource-configuration-unavailable";
			return response;
		}
		BaseContainer configuration = configurations.Get(0);
		if (!configuration)
		{
			response.status = "resource-configuration-unavailable";
			return response;
		}
		response.found = true;
		response.status = "found";
		response.resourceName = meta.GetResourceID();
		response.className = configuration.GetClassName();
		array<string> sourceAddons = new array<string>();
		meta.GetSourceAddons(sourceAddons);
		int sourceAddonCount = 0;
		foreach (string sourceAddon : sourceAddons)
		{
			if (sourceAddonCount >= 64)
			{
				response.sourceAddonsTruncated = true;
				break;
			}
			if (response.sourceAddons != string.Empty)
				response.sourceAddons += ";";
			response.sourceAddons += sourceAddon;
			sourceAddonCount++;
			if (response.sourceAddons.Length() >= 4096)
			{
				response.sourceAddonsTruncated = true;
				break;
			}
		}
		return response;
	}
}
#endif
