#ifdef WORKBENCH
class RST_WorkbenchLayerStateRequest : JsonApiStruct
{
	int subScene;
	int layerId;

	void RST_WorkbenchLayerStateRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchLayerStateResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	int subScene;
	int layerId;
	string layerPath;
	bool visible;
	bool explicitlyLocked;
	bool lockedInHierarchy;

	void RST_WorkbenchLayerStateResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchLayerState : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchLayerStateRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchLayerStateRequest typedRequest = RST_WorkbenchLayerStateRequest.Cast(request);
		RST_WorkbenchLayerStateResponse response = new RST_WorkbenchLayerStateResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.subScene = typedRequest.subScene;
		response.layerId = typedRequest.layerId;
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor || !editor.GetApi())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (typedRequest.subScene < 0 || typedRequest.subScene >= api.GetNumSubScenes() || typedRequest.layerId < 0)
		{
			response.status = "invalid-layer";
			return response;
		}
		response.layerPath = api.GetSubsceneLayerPath(typedRequest.subScene, typedRequest.layerId);
		if (response.layerPath.IsEmpty())
		{
			response.status = "layer-not-found";
			return response;
		}
		response.visible = api.IsEntityLayerVisible(typedRequest.subScene, typedRequest.layerId);
		response.explicitlyLocked = api.IsEntityLayerLocked(typedRequest.subScene, typedRequest.layerId);
		response.lockedInHierarchy = api.IsEntityLayerLockedHierarchy(typedRequest.subScene, typedRequest.layerId);
		response.status = "available";
		return response;
	}
}
#endif
