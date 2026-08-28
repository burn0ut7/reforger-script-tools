#ifdef WORKBENCH
class RST_WorkbenchSelectedEntityHierarchyRequest : JsonApiStruct
{
	int selectionIndex;

	void RST_WorkbenchSelectedEntityHierarchyRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchSelectedEntityHierarchyResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	bool editorAvailable;
	string status;
	string entity;
	string ancestors;
	bool ancestorsTruncated;
	string children;
	bool childrenTruncated;

	void RST_WorkbenchSelectedEntityHierarchyResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchSelectedEntityHierarchy : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchSelectedEntityHierarchyRequest();
	}

	protected void AppendEntity(out string records, WorldEditorAPI api, IEntitySource entity)
	{
		if (!entity)
			return;
		if (records != string.Empty)
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
		RST_WorkbenchSelectedEntityHierarchyRequest typedRequest = RST_WorkbenchSelectedEntityHierarchyRequest.Cast(request);
		RST_WorkbenchSelectedEntityHierarchyResponse response = new RST_WorkbenchSelectedEntityHierarchyResponse();
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
		if (typedRequest.selectionIndex < 0 || typedRequest.selectionIndex >= worldEditorApi.GetSelectedEntitiesCount())
		{
			response.status = "selection-index-out-of-range";
			return response;
		}
		IEntitySource entity = worldEditorApi.GetSelectedEntity(typedRequest.selectionIndex);
		if (!entity)
		{
			response.status = "selected-entity-unavailable";
			return response;
		}
		response.status = "available";
		AppendEntity(response.entity, worldEditorApi, entity);
		BaseContainer parent = entity.GetParent();
		for (int index = 0; parent && index < 32; index++)
		{
			IEntitySource parentEntity = IEntitySource.Cast(parent);
			if (parentEntity)
				AppendEntity(response.ancestors, worldEditorApi, parentEntity);
			parent = parent.GetParent();
		}
		response.ancestorsTruncated = parent != null;
		int childCount = entity.GetNumChildren();
		int returnedCount = 0;
		for (int index = 0; index < childCount; index++)
		{
			IEntitySource childEntity = IEntitySource.Cast(entity.GetChild(index));
			if (!childEntity)
				continue;
			if (returnedCount >= 64)
			{
				response.childrenTruncated = true;
				break;
			}
			AppendEntity(response.children, worldEditorApi, childEntity);
			returnedCount++;
		}
		return response;
	}
}
#endif
