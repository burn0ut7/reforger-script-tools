#ifdef WORKBENCH
class RST_WorkbenchListEntitiesRequest : JsonApiStruct
{
	string query;
	string className;
	int subScene;
	int layerId;
	int offset;
	int limit;

	void RST_WorkbenchListEntitiesRequest()
	{
		RegAll();
		subScene = -1;
		layerId = -1;
	}
}

class RST_WorkbenchListEntitiesResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string worldPath;
	string entities;
	bool hasMore;

	void RST_WorkbenchListEntitiesResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchListEntities : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchListEntitiesRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchListEntitiesRequest typedRequest = RST_WorkbenchListEntitiesRequest.Cast(request);
		RST_WorkbenchListEntitiesResponse response = new RST_WorkbenchListEntitiesResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		WorldEditor worldEditor = Workbench.GetModule(WorldEditor);
		if (!worldEditor)
			return response;
		WorldEditorAPI api = worldEditor.GetApi();
		if (!api || typedRequest.offset < 0 || typedRequest.limit < 1 || typedRequest.limit > 100)
			return response;
		api.GetWorldPath(response.worldPath);
		int matched = 0;
		int returned = 0;
		for (int index = 0, count = api.GetEditorEntityCount(); index < count; index++)
		{
			IEntitySource entity = api.GetEditorEntity(index);
			if (!entity)
				continue;
			if (typedRequest.subScene >= 0 && entity.GetSubScene() != typedRequest.subScene)
				continue;
			if (typedRequest.layerId >= 0 && entity.GetLayerID() != typedRequest.layerId)
				continue;
			string name = api.GetEntityNiceName(entity);
			if (!typedRequest.className.IsEmpty() && entity.GetClassName().IndexOf(typedRequest.className) == -1)
				continue;
			if (!typedRequest.query.IsEmpty() && name.IndexOf(typedRequest.query) == -1 && entity.GetClassName().IndexOf(typedRequest.query) == -1)
				continue;
			if (matched++ < typedRequest.offset)
				continue;
			if (returned >= typedRequest.limit)
			{
				response.hasMore = true;
				break;
			}
			if (!response.entities.IsEmpty())
				response.entities += ";";
			IEntity runtimeEntity = api.SourceToEntity(entity);
			if (!runtimeEntity)
				response.entities += string.Format("%1|%2|%3|%4", entity.GetID().ToString(), entity.GetClassName(), entity.GetSubScene(), entity.GetLayerID());
			else
			{
				vector transform[4];
				runtimeEntity.GetTransform(transform);
				string resourceName = string.Format("%1", entity.GetResourceName());
				string authoredName = entity.GetName();
				string subSceneName = runtimeEntity.GetWorld().GetSubSceneName(entity.GetSubScene());
				string layerName = api.GetEntitySubsceneLayer(entity.GetSubScene(), entity);
				if (authoredName == resourceName)
					authoredName = string.Empty;
				resourceName.Replace("|", "/");
				resourceName.Replace(";", "/");
				authoredName.Replace("|", "/");
				authoredName.Replace(";", "/");
				subSceneName.Replace("|", "/");
				subSceneName.Replace(";", "/");
				layerName.Replace("|", "/");
				layerName.Replace(";", "/");
				response.entities += string.Format("%1|%2|%3|%4|%5|%6|%7", entity.GetID().ToString(), entity.GetClassName(), entity.GetSubScene(), entity.GetLayerID(), transform[3][0], transform[3][1], transform[3][2]) + "|" + resourceName + "|" + authoredName + "|" + subSceneName + "|" + layerName;
			}
			returned++;
		}
		return response;
	}
}
#endif
