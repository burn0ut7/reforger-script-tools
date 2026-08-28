#ifdef WORKBENCH
class RST_WorkbenchListResourcesRequest : JsonApiStruct
{
	string extensions;
	string query;
	string rootPath;
	string addonGuid;
	int offset;
	int limit;

	void RST_WorkbenchListResourcesRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchListResourcesResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string loadedAddons;
	string resources;
	string resourceDetails;
	bool hasMore;

	void RST_WorkbenchListResourcesResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchListResources : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchListResourcesRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchListResourcesRequest typedRequest = RST_WorkbenchListResourcesRequest.Cast(request);
		RST_WorkbenchListResourcesResponse response = new RST_WorkbenchListResourcesResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		array<string> addonGuids = new array<string>();
		GameProject.GetLoadedAddons(addonGuids);
		foreach (string addonGuid : addonGuids)
		{
			string addonId = GameProject.GetAddonID(addonGuid);
			if (addonId == string.Empty)
				continue;
			if (response.loadedAddons != string.Empty)
				response.loadedAddons += ";";
			response.loadedAddons += addonId;
		}
		array<string> extensions = new array<string>();
		if (typedRequest.extensions != string.Empty)
			typedRequest.extensions.Split(";", extensions, true);
		array<string> searchStrings = new array<string>();
		if (typedRequest.query != string.Empty)
			searchStrings.Insert(typedRequest.query);
		SearchResourcesFilter filter = new SearchResourcesFilter();
		filter.fileExtensions = extensions;
		filter.searchStr = searchStrings;
		filter.rootPath = typedRequest.rootPath;
		array<ResourceName> allResources = new array<ResourceName>();
		ResourceDatabase.SearchResources(filter, allResources.Insert);
		if (!typedRequest.addonGuid.IsEmpty())
		{
			array<ResourceName> matchingResources = new array<ResourceName>();
			foreach (ResourceName resource : allResources)
			{
				string resourceName = string.Format("%1", resource);
				if (resourceName.Contains("{" + typedRequest.addonGuid + "}"))
					matchingResources.Insert(resource);
			}
			allResources = matchingResources;
		}
		allResources.Sort();
		int start = typedRequest.offset;
		if (start < 0)
			start = 0;
		int limit = typedRequest.limit;
		if (limit < 1)
			limit = 1;
		if (limit > 200)
			limit = 200;
		int returnedCount = 0;
		for (int index = start; index < allResources.Count() && returnedCount < limit; index++)
		{
			ResourceName resource = allResources[index];
			string resourceName = string.Format("%1", resource);
			resourceName.Replace(";", "/");
			int addonGuidEnd = resourceName.IndexOf("}");
			if (!resourceName.StartsWith("{") || addonGuidEnd <= 1)
				continue;
			string addonGuid = resourceName.Substring(1, addonGuidEnd - 1);
			string addonId = GameProject.GetAddonID(addonGuid);
			if (addonId.IsEmpty())
				addonId = GameProject.GetAddonID(resourceName.Substring(0, addonGuidEnd + 1));
			string logicalPath = resource.GetPath();
			string extension;
			FilePath.StripExtension(logicalPath, extension);
			if (logicalPath.IsEmpty() || extension.IsEmpty())
				continue;
			if (!response.resources.IsEmpty())
			{
				response.resources += ";";
				response.resourceDetails += ";";
			}
			response.resources += resourceName;
			response.resourceDetails += resourceName + "|" + addonGuid + "|" + addonId + "|" + logicalPath + "|" + extension;
			returnedCount++;
		}
		response.hasMore = start + returnedCount < allResources.Count();
		return response;
	}
}
#endif
