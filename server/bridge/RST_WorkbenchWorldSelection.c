#ifdef WORKBENCH
class RST_WorkbenchWorldSelectionRequest : JsonApiStruct
{
	void RST_WorkbenchWorldSelectionRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchWorldSelectionResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	bool editorAvailable;
	string status;
	int selectedCount;
	string selectedEntities;
	bool selectedEntitiesTruncated;

	void RST_WorkbenchWorldSelectionResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchWorldSelection : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchWorldSelectionRequest();
	}

	protected void AppendEntity(out string records, WorldEditorAPI api, IEntitySource entity)
	{
		if (!entity)
			return;
		if (!records.IsEmpty())
			records += ";";
		IEntity runtimeEntity = api.SourceToEntity(entity);
		if (!runtimeEntity)
		{
			records += string.Format("%1|%2|%3|%4", entity.GetID().ToString(), entity.GetClassName(), entity.GetSubScene(), entity.GetLayerID());
			return;
		}
		vector transform[4];
		runtimeEntity.GetTransform(transform);
		string resourceName = string.Format("%1", entity.GetResourceName());
		string name = entity.GetName();
		string subSceneName = runtimeEntity.GetWorld().GetSubSceneName(entity.GetSubScene());
		string layerName = api.GetEntitySubsceneLayer(entity.GetSubScene(), entity);
		if (name == resourceName)
			name = string.Empty;
		resourceName.Replace("|", "/");
		resourceName.Replace(";", "/");
		name.Replace("|", "/");
		name.Replace(";", "/");
		subSceneName.Replace("|", "/");
		subSceneName.Replace(";", "/");
		layerName.Replace("|", "/");
		layerName.Replace(";", "/");
		records += string.Format("%1|%2|%3|%4|%5|%6|%7", entity.GetID().ToString(), entity.GetClassName(), entity.GetSubScene(), entity.GetLayerID(), transform[3][0], transform[3][1], transform[3][2]) + "|" + resourceName + "|" + name + "|" + subSceneName + "|" + layerName;
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchWorldSelectionResponse response = new RST_WorkbenchWorldSelectionResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		WorldEditor worldEditor = Workbench.GetModule(WorldEditor);
		if (!worldEditor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI worldEditorApi = worldEditor.GetApi();
		if (!worldEditorApi)
		{
			response.status = "world-editor-api-unavailable";
			return response;
		}
		response.editorAvailable = true;
		response.status = "available";
		response.selectedCount = worldEditorApi.GetSelectedEntitiesCount();
		int boundedCount = response.selectedCount;
		if (boundedCount > 32)
		{
			boundedCount = 32;
			response.selectedEntitiesTruncated = true;
		}
		for (int index = 0; index < boundedCount; index++)
		{
			IEntitySource entity = worldEditorApi.GetSelectedEntity(index);
			if (!entity)
				continue;
			AppendEntity(response.selectedEntities, worldEditorApi, entity);
		}
		return response;
	}
}
#endif
