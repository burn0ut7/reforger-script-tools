#ifdef WORKBENCH
class RST_WorkbenchPropertiesRequest : JsonApiStruct
{
	string entityId;
	string propertyName;
	string expectedValue;
	string value;

	void RST_WorkbenchPropertiesRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchPropertiesResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	string properties;

	void RST_WorkbenchPropertiesResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchListEntityProperties : RST_WorkbenchEntityMutationBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchPropertiesRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchPropertiesRequest r = RST_WorkbenchPropertiesRequest.Cast(request);
		RST_WorkbenchPropertiesResponse response = new RST_WorkbenchPropertiesResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor || !editor.GetApi())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		IEntitySource entity = Find(editor.GetApi(), r.entityId);
		if (!entity)
		{
			response.status = "entity-not-found";
			return response;
		}
		for (int i, count = entity.GetNumVars(); i < count; i++)
		{
			string name = entity.GetVarName(i);
			DataVarType dataType = entity.GetDataVarType(i);
			string typeName = PropertyTypeName(dataType);
			string value;
			if (typeName.IsEmpty() || !ReadPropertyValue(entity, name, dataType, value))
				continue;
			value.Replace("|", "/");
			value.Replace(";", "/");
			if (!response.properties.IsEmpty())
				response.properties += ";";
			response.properties += string.Format("%1|%2|%3|1", name, typeName, value);
		}
		response.status = "available";
		return response;
	}
}

class RST_WorkbenchSetEntityProperty : RST_WorkbenchEntityMutationBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchPropertiesRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchPropertiesRequest r = RST_WorkbenchPropertiesRequest.Cast(request);
		RST_WorkbenchEntityMutationResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!Setup(api, response))
			return response;
		IEntitySource entity = Find(api, r.entityId);
		if (!entity || r.propertyName.IsEmpty())
		{
			response.status = "property-not-found";
			return response;
		}
		int propertyIndex = entity.GetVarIndex(r.propertyName);
		if (propertyIndex < 0)
		{
			response.status = "property-not-found";
			return response;
		}
		string observedValue;
		if (!ReadPropertyValue(entity, r.propertyName, entity.GetDataVarType(propertyIndex), observedValue)    || observedValue != r.expectedValue)
		{
			response.status = "stale-property-observation";
			return response;
		}
		if (api.IsEntityLayerLockedHierarchy(entity.GetSubScene(), entity.GetLayerID())    || !api.BeginEntityAction("Reforger Script Tools: set entity property"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		if (!api.SetVariableValue(entity, null, r.propertyName, r.value))
		{
			api.EndEntityAction("Reforger Script Tools: set entity property");
			response.status = "mutation-rejected";
			return response;
		}
		api.EndEntityAction("Reforger Script Tools: set entity property");
		Record(api, response, entity);
		response.status = "property-set";
		return response;
	}
}
#endif
