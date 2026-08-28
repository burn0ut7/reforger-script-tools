#ifdef WORKBENCH
class RST_WorkbenchSetSelectionRequest : JsonApiStruct
{
	string entityId;

	void RST_WorkbenchSetSelectionRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchSetSelectionResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	string entity;

	void RST_WorkbenchSetSelectionResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchSetSelection : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchSetSelectionRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchSetSelectionRequest typedRequest = RST_WorkbenchSetSelectionRequest.Cast(request);
		RST_WorkbenchSetSelectionResponse response = new RST_WorkbenchSetSelectionResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!api)
		{
			response.status = "world-editor-api-unavailable";
			return response;
		}
		IEntitySource target;
		for (int index = 0, count = api.GetEditorEntityCount(); index < count; index++)
		{
			IEntitySource candidate = api.GetEditorEntity(index);
			if (candidate && candidate.GetID().ToString() == typedRequest.entityId)
			{
				target = candidate;
				break;
			}
		}
		if (!target)
		{
			response.status = "entity-not-found";
			return response;
		}
		api.SetEntitySelection(target);
		response.status = "selected";
		IEntity runtimeEntity = api.SourceToEntity(target);
		if (!runtimeEntity)
			response.entity = string.Format("%1|%2|%3|%4", target.GetID().ToString(), target.GetClassName(), target.GetSubScene(), target.GetLayerID());
		else
		{
			vector transform[4];
			runtimeEntity.GetTransform(transform);
			string resourceName = string.Format("%1", target.GetResourceName());
			string name = target.GetName();
			string subSceneName = runtimeEntity.GetWorld().GetSubSceneName(target.GetSubScene());
			string layerName = api.GetEntitySubsceneLayer(target.GetSubScene(), target);
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
			response.entity = string.Format("%1|%2|%3|%4|%5|%6|%7", target.GetID().ToString(), target.GetClassName(), target.GetSubScene(), target.GetLayerID(), transform[3][0], transform[3][1], transform[3][2]) + "|" + resourceName + "|" + name + "|" + subSceneName + "|" + layerName;
		}
		return response;
	}
}
#endif
