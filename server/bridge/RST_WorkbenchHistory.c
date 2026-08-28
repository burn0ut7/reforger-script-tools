#ifdef WORKBENCH
class RST_WorkbenchHistoryResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string operation;
	string status;
	bool historyAvailable;
	bool changed;

	void RST_WorkbenchHistoryResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchHistoryRequest : JsonApiStruct
{
}

class RST_WorkbenchUndo : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchHistoryRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchHistoryResponse response = new RST_WorkbenchHistoryResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.operation = "undo";
		WorldEditor worldEditor = Workbench.GetModule(WorldEditor);
		if (!worldEditor)
		{
			response.status = "world-editor-unavailable";
			response.historyAvailable = false;
			response.changed = false;
			return response;
		}
		array<string> menuPath = {"Edit", "Undo"};
		bool accepted = worldEditor.ExecuteAction(menuPath, true);
		if (accepted)
			response.status = "invoked";
		else
			response.status = "history-unavailable";
		response.historyAvailable = accepted;
		response.changed = accepted;
		return response;
	}
}

class RST_WorkbenchRedo : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchHistoryRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchHistoryResponse response = new RST_WorkbenchHistoryResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.operation = "redo";
		WorldEditor worldEditor = Workbench.GetModule(WorldEditor);
		if (!worldEditor)
		{
			response.status = "world-editor-unavailable";
			response.historyAvailable = false;
			response.changed = false;
			return response;
		}
		array<string> menuPath = {"Edit", "Redo"};
		bool accepted = worldEditor.ExecuteAction(menuPath, true);
		if (accepted)
			response.status = "invoked";
		else
			response.status = "history-unavailable";
		response.historyAvailable = accepted;
		response.changed = accepted;
		return response;
	}
}
#endif
