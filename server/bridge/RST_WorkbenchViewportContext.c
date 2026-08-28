#ifdef WORKBENCH
class RST_WorkbenchViewportContextRequest : JsonApiStruct
{
	void RST_WorkbenchViewportContextRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchViewportContextResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	int width;
	int height;
	int mouseX;
	int mouseY;
	bool mouseInside;
	float cameraX;
	float cameraY;
	float cameraZ;
	float cameraDirectionX;
	float cameraDirectionY;
	float cameraDirectionZ;
	float startX;
	float startY;
	float startZ;
	float endX;
	float endY;
	float endZ;
	float directionX;
	float directionY;
	float directionZ;

	void RST_WorkbenchViewportContextResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchViewportContext : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchViewportContextRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchViewportContextResponse response = new RST_WorkbenchViewportContextResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		WorldEditor e = Workbench.GetModule(WorldEditor);
		if (!e || !e.GetApi() || !e.GetApi().GetWorld())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI a = e.GetApi();
		BaseWorld world = a.GetWorld();
		vector cameraTransform[4];
		world.GetCurrentCamera(cameraTransform);
		response.cameraX = cameraTransform[3][0];
		response.cameraY = cameraTransform[3][1];
		response.cameraZ = cameraTransform[3][2];
		response.cameraDirectionX = cameraTransform[2][0];
		response.cameraDirectionY = cameraTransform[2][1];
		response.cameraDirectionZ = cameraTransform[2][2];
		response.width = a.GetScreenWidth();
		response.height = a.GetScreenHeight();
		response.mouseX = a.GetMousePosX(false);
		response.mouseY = a.GetMousePosY(false);
		response.mouseInside = response.mouseX >= 0 && response.mouseY >= 0 && response.mouseX < response.width && response.mouseY < response.height;
		if (!response.mouseInside)
		{
			response.status = "mouse-outside-viewport";
			return response;
		}
		vector start, end, direction;
		if (!a.TraceWorldPos(response.mouseX, response.mouseY, TraceFlags.WORLD, start, end, direction))
		{
			response.status = "mouse-world-position-unavailable";
			return response;
		}
		response.startX = start[0];
		response.startY = start[1];
		response.startZ = start[2];
		response.endX = end[0];
		response.endY = end[1];
		response.endZ = end[2];
		response.directionX = direction[0];
		response.directionY = direction[1];
		response.directionZ = direction[2];
		response.status = "available";
		return response;
	}
}
#endif
