#ifdef WORKBENCH
class RST_WorkbenchClearSelectionRequest : JsonApiStruct
{
	void RST_WorkbenchClearSelectionRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchClearSelectionResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	bool editorAvailable;
	string status;
	int selectedCount;
	string selectedEntities;
	bool selectedEntitiesTruncated;

	void RST_WorkbenchClearSelectionResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchClearSelection : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchClearSelectionRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchClearSelectionResponse response = new RST_WorkbenchClearSelectionResponse();
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
		response.editorAvailable = true;
		api.ClearEntitySelection();
		api.UpdateSelectionGui();
		response.selectedCount = api.GetSelectedEntitiesCount();
		if (response.selectedCount == 0)
			response.status = "available";
		else
			response.status = "selection-not-observed";
		return response;
	}
}
#endif
