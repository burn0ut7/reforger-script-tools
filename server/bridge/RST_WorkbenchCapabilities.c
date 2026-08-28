#ifdef WORKBENCH
class RST_WorkbenchCapabilitiesRequest : JsonApiStruct
{
	void RST_WorkbenchCapabilitiesRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchCapabilitiesResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	int runtimeGeneration;
	string capabilities;

	void RST_WorkbenchCapabilitiesResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchCapabilities : NetApiHandler
{
	static int s_RuntimeGeneration;

	void RST_WorkbenchCapabilities()
	{
		if (s_RuntimeGeneration == 0)
			s_RuntimeGeneration = System.GetTickCount();

		if (s_RuntimeGeneration == 0)
			s_RuntimeGeneration = 1;
	}

	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchCapabilitiesRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchCapabilitiesResponse response = new RST_WorkbenchCapabilitiesResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.runtimeGeneration = s_RuntimeGeneration;
		response.capabilities = "state;editors;open-resource;play-session;project-context;loaded-addon-graph;inspect-resource;world-selection;entity-hierarchy;list-resources;list-entities;layer-state;inspect-entity;set-selection;clear-selection;entity-position;entity-details;create-entity;rename-entity;delete-entity;move-entity;rotate-entity;reparent-entity;duplicate-entity;entity-properties;components;component-properties;reload-action";
		return response;
	}
}
#endif
