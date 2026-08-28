#ifdef WORKBENCH
class RST_WorkbenchComponentsRequest : JsonApiStruct
{
	string entityId;
	string componentId;
	string className;
	string propertyName;
	string expectedValue;
	string value;
	bool confirm;

	void RST_WorkbenchComponentsRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchComponentsResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	string entity;
	string components;
	string properties;

	void RST_WorkbenchComponentsResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchComponentsBase : NetApiHandler
{
	IEntitySource Find(WorldEditorAPI api, string id)
	{
		for (int i = 0, count = api.GetEditorEntityCount(); i < count; i++)
		{
			IEntitySource candidate = api.GetEditorEntity(i);
			if (candidate && candidate.GetID().ToString() == id)
				return candidate;
		}
		return null;
	}

	RST_WorkbenchComponentsResponse Response()
	{
		RST_WorkbenchComponentsResponse response = new RST_WorkbenchComponentsResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		return response;
	}

	// Workbench exposes authored direct components through the `components` container in
	// some editor contexts, even when IEntitySource.GetComponentCount() is zero.
	int ComponentCount(IEntitySource entity)
	{
		int count = entity.GetComponentCount();
		if (count == 0)
		{
			ref BaseContainerList components = entity.GetObjectArray("components");
			if (components)
				count = components.Count();
		}
		return count;
	}

	IEntityComponentSource ComponentAt(IEntitySource entity, int index)
	{
		IEntityComponentSource component;
		int count = entity.GetComponentCount();
		if (count > 0)
			component = entity.GetComponent(index);
		else
		{
			ref BaseContainerList components = entity.GetObjectArray("components");
			if (components)
				component = IEntityComponentSource.Cast(components.Get(index));
		}
		return component;
	}

	void List(IEntitySource entity, RST_WorkbenchComponentsResponse response)
	{
		int count = ComponentCount(entity);
		for (int i; i < count; i++)
		{
			IEntityComponentSource component = ComponentAt(entity, i);
			if (!component)
				continue;
			if (!response.components.IsEmpty())
				response.components += ";";
			response.components += string.Format("%1|%2", i, component.GetClassName());
		}
	}

	string SupportedPropertyType(DataVarType dataType)
	{
		switch (dataType)
		{
			case DataVarType.BOOLEAN: return "bool";
			case DataVarType.INTEGER: return "integer";
			case DataVarType.SCALAR: return "float";
			case DataVarType.VECTOR3: return "vector";
			case DataVarType.STRING: return "string";
			case DataVarType.RESOURCE_NAME: return "resource";
		}
		return string.Empty;
	}

	string PropertyOrigin(IEntityComponentSource component, string name)
	{
		if (component.IsVariableSetDirectly(name))
			return "direct";
		for (BaseContainer ancestor = component.GetAncestor(); ancestor; ancestor = ancestor.GetAncestor())
		{
			if (ancestor.IsVariableSetDirectly(name))
				return "inherited";
		}
		return "default";
	}

	bool ReadPropertyValue(IEntityComponentSource component, string name, DataVarType dataType, out string value)
	{
		bool boolValue;
		int integerValue;
		float floatValue;
		vector vectorValue;
		string stringValue;
		switch (dataType)
		{
			case DataVarType.BOOLEAN:     if (!component.Get(name, boolValue)) return false;
			if (boolValue)
				value = "1";
			else
				value = "0";
			return true;
			case DataVarType.INTEGER:     if (!component.Get(name, integerValue)) return false;
			value = integerValue.ToString();
			return true;
			case DataVarType.SCALAR:     if (!component.Get(name, floatValue)) return false;
			value = floatValue.ToString();
			return true;
			case DataVarType.VECTOR3:     if (!component.Get(name, vectorValue)) return false;
			value = string.Format("%1 %2 %3", vectorValue[0], vectorValue[1], vectorValue[2]);
			return true;
			case DataVarType.STRING:    case DataVarType.RESOURCE_NAME:     if (!component.Get(name, stringValue)) return false;
			value = stringValue;
			return true;
		}
		return false;
	}

	void ListProperties(IEntityComponentSource component, RST_WorkbenchComponentsResponse response)
	{
		for (int i = 0, count = component.GetNumVars(); i < count; i++)
		{
			string name = component.GetVarName(i);
			DataVarType dataType = component.GetDataVarType(i);
			string typeName = SupportedPropertyType(dataType);
			string value;
			string directlySet;
			if (typeName.IsEmpty() || !ReadPropertyValue(component, name, dataType, value))
				continue;
			if (component.IsVariableSetDirectly(name))
				directlySet = "1";
			else
				directlySet = "0";
			value.Replace("|", "/");
			value.Replace(";", "/");
			if (!response.properties.IsEmpty())
				response.properties += ";";
			response.properties += string.Format(     "%1|%2|%3|%4|%5",     name,     typeName,     value,     directlySet,     PropertyOrigin(component, name));
		}
	}

	IEntityComponentSource FindComponent(IEntitySource entity, string componentId)
	{
		for (int i = 0, count = ComponentCount(entity); i < count; i++)
		{
			IEntityComponentSource component = ComponentAt(entity, i);
			if (component && componentId == string.Format("cmp1:%1:%2", i, component.GetClassName()))
				return component;
		}
		return null;
	}
}

class RST_WorkbenchListComponents : RST_WorkbenchComponentsBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchComponentsRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchComponentsRequest r = RST_WorkbenchComponentsRequest.Cast(request);
		RST_WorkbenchComponentsResponse response = Response();
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
		List(entity, response);
		response.status = "available";
		return response;
	}
}

class RST_WorkbenchInspectComponent : RST_WorkbenchComponentsBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchComponentsRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchComponentsRequest r = RST_WorkbenchComponentsRequest.Cast(request);
		RST_WorkbenchComponentsResponse response = Response();
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
		List(entity, response);
		for (int i = 0, count = ComponentCount(entity); i < count; i++)
		{
			IEntityComponentSource component = ComponentAt(entity, i);
			if (component && r.componentId == string.Format("cmp1:%1:%2", i, component.GetClassName()))
			{
				ListProperties(component, response);
				response.status = "available";
				return response;
			}
		}
		response.status = "component-not-found";
		return response;
	}
}

class RST_WorkbenchAddComponent : RST_WorkbenchComponentsBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchComponentsRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchComponentsRequest r = RST_WorkbenchComponentsRequest.Cast(request);
		RST_WorkbenchComponentsResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		WorldEditorAPI api;
		IEntitySource entity;
		typename componentType;
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		api = editor.GetApi();
		if (!api || api.IsPrefabEditMode() || api.IsDoingEditAction())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		entity = Find(api, r.entityId);
		componentType = r.className.ToType();
		if (!entity || r.className.IsEmpty() || !componentType || !componentType.IsInherited(ScriptComponent) || api.IsEntityLayerLockedHierarchy(entity.GetSubScene(), entity.GetLayerID()) || !api.BeginEntityAction("Reforger Script Tools: add component"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		if (!api.CreateComponent(entity, r.className))
		{
			api.EndEntityAction("Reforger Script Tools: add component");
			response.status = "mutation-rejected";
			return response;
		}
		api.EndEntityAction("Reforger Script Tools: add component");
		List(entity, response);
		response.status = "added";
		return response;
	}
}

class RST_WorkbenchRemoveComponent : RST_WorkbenchComponentsBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchComponentsRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchComponentsRequest r = RST_WorkbenchComponentsRequest.Cast(request);
		RST_WorkbenchComponentsResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		WorldEditorAPI api;
		IEntitySource entity;
		IEntityComponentSource component;
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		api = editor.GetApi();
		if (!api || api.IsPrefabEditMode() || api.IsDoingEditAction())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		entity = Find(api, r.entityId);
		if (!entity)
		{
			response.status = "component-not-found";
			return response;
		}
		component = FindComponent(entity, r.componentId);
		if (!component)
		{
			response.status = "component-not-found";
			return response;
		}
		if (!r.confirm)
		{
			List(entity, response);
			response.status = "confirmation-required";
			return response;
		}
		if (api.IsEntityLayerLockedHierarchy(entity.GetSubScene(), entity.GetLayerID()) || !api.BeginEntityAction("Reforger Script Tools: remove component"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		if (!api.DeleteComponent(entity, component))
		{
			api.EndEntityAction("Reforger Script Tools: remove component");
			response.status = "mutation-rejected";
			return response;
		}
		api.EndEntityAction("Reforger Script Tools: remove component");
		List(entity, response);
		response.status = "removed";
		return response;
	}
}

class RST_WorkbenchSetComponentProperty : RST_WorkbenchComponentsBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchComponentsRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchComponentsRequest r = RST_WorkbenchComponentsRequest.Cast(request);
		RST_WorkbenchComponentsResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!api || api.IsPrefabEditMode() || api.IsDoingEditAction())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		IEntitySource entity = Find(api, r.entityId);
		if (!entity || api.IsEntityLayerLockedHierarchy(entity.GetSubScene(), entity.GetLayerID()))
		{
			response.status = "mutation-rejected";
			return response;
		}
		IEntityComponentSource component = FindComponent(entity, r.componentId);
		if (!component)
		{
			response.status = "property-not-found";
			return response;
		}
		int propertyIndex = component.GetVarIndex(r.propertyName);
		if (r.propertyName.IsEmpty() || propertyIndex < 0)
		{
			response.status = "property-not-found";
			return response;
		}
		string observedValue;
		DataVarType propertyDataType = component.GetDataVarType(propertyIndex);
		if (!ReadPropertyValue(component, r.propertyName, propertyDataType, observedValue)    || observedValue != r.expectedValue)
		{
			response.status = "stale-property-observation";
			return response;
		}
		int componentIndex = -1;
		for (int i = 0, count = ComponentCount(entity); i < count; i++)
		{
			if (ComponentAt(entity, i) == component)
			{
				componentIndex = i;
				break;
			}
		}
		if (componentIndex < 0 || !api.BeginEntityAction("Reforger Script Tools: set component property"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		if (!api.SetVariableValue(    entity,
		{
			new ContainerIdPathEntry("components", componentIndex)
		}
		,    r.propertyName,    r.value))
		{
			api.EndEntityAction("Reforger Script Tools: set component property");
			response.status = "mutation-rejected";
			return response;
		}
		api.EndEntityAction("Reforger Script Tools: set component property");
		List(entity, response);
		ListProperties(component, response);
		response.status = "property-set";
		return response;
	}
}

class RST_WorkbenchSetPrefabComponentProperty : RST_WorkbenchComponentsBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchComponentsRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchComponentsRequest r = RST_WorkbenchComponentsRequest.Cast(request);
		RST_WorkbenchComponentsResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor || !editor.GetApi())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!api.IsPrefabEditMode() || api.IsDoingEditAction())
		{
			response.status = "prefab-edit-unavailable";
			return response;
		}
		IEntitySource entity = Find(api, r.entityId);
		IEntityComponentSource component;
		if (entity)
			component = FindComponent(entity, r.componentId);
		int index = -1;
		if (component)
			index = component.GetVarIndex(r.propertyName);
		string observed;
		if (!component || index < 0 || !ReadPropertyValue(component, r.propertyName, component.GetDataVarType(index), observed) || observed != r.expectedValue)
		{
			response.status = "stale-property-observation";
			return response;
		}
		int componentIndex = -1;
		for (int i, count = ComponentCount(entity); i < count; i++)
		{
			if (ComponentAt(entity, i) == component)
			{
				componentIndex = i;
				break;
			}
		}
		if (componentIndex < 0 || !api.BeginEntityAction("Reforger Script Tools: set prefab component property") || !api.SetVariableValue(entity,
		{
			new ContainerIdPathEntry("components", componentIndex)
		}
		, r.propertyName, r.value))
		{
			api.EndEntityAction("Reforger Script Tools: set prefab component property");
			response.status = "mutation-rejected";
			return response;
		}
		api.EndEntityAction("Reforger Script Tools: set prefab component property");
		List(entity, response);
		ListProperties(component, response);
		response.status = "prefab-component-property-set";
		return response;
	}
}
#endif
