#ifdef WORKBENCH
class RST_WorkbenchSearchEntitiesRequest : JsonApiStruct
{
	string query;
	string className;
	string resourceQuery;
	string componentClasses;
	string relationDirection;
	string relationClassName;
	string relationComponentClasses;
	int relationMaxDepth;
	int subScene;
	int layerId;
	int offset;
	int limit;

	void RST_WorkbenchSearchEntitiesRequest()
	{
		RegAll();
		subScene = -1;
		layerId = -1;
	}
}

class RST_WorkbenchSearchEntitiesResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	string worldPath;
	string results;
	int totalMatches;
	int namedMatches;
	bool hasMore;
	bool relationTraversalTruncated;

	void RST_WorkbenchSearchEntitiesResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchSearchEntities : NetApiHandler
{
	static const int MAX_RELATION_CANDIDATES = 4096;
	static const int MAX_RESULT_CHARACTERS = 262144;
	static const int MAX_RESULT_FIELD_CHARACTERS = 4096;

	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchSearchEntitiesRequest();
	}

	protected string BoundResultField(string value)
	{
		if (value.Length() > MAX_RESULT_FIELD_CHARACTERS)
			return value.Substring(0, MAX_RESULT_FIELD_CHARACTERS);
		return value;
	}

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
		if (entity.GetComponentCount() > 0)
			return entity.GetComponent(index);
		ref BaseContainerList components = entity.GetObjectArray("components");
		if (components)
			return IEntityComponentSource.Cast(components.Get(index));
		return null;
	}

	protected bool HasComponent(IEntitySource entity, string expected)
	{
		for (int index, count = ComponentCount(entity); index < count; index++)
		{
			IEntityComponentSource component = ComponentAt(entity, index);
			if (component && component.GetClassName() == expected)
				return true;
		}
		return false;
	}

	protected bool HasRequiredComponents(IEntitySource entity, array<string> required)
	{
		foreach (string expected : required)
		{
			if (!HasComponent(entity, expected))
				return false;
		}
		return true;
	}

	protected bool MatchesCandidate(IEntitySource entity, RST_WorkbenchSearchEntitiesRequest request, array<string> required)
	{
		if (!entity)
			return false;
		if (request.subScene >= 0 && entity.GetSubScene() != request.subScene)
			return false;
		if (request.layerId >= 0 && entity.GetLayerID() != request.layerId)
			return false;
		string name = entity.GetName();
		string resource = string.Format("%1", entity.GetResourceName());
		string className = entity.GetClassName();
		if (!request.query.IsEmpty() && !name.Contains(request.query) && !className.Contains(request.query) && !resource.Contains(request.query))
			return false;
		if (!request.className.IsEmpty() && className != request.className)
			return false;
		if (!request.resourceQuery.IsEmpty() && !resource.Contains(request.resourceQuery))
			return false;
		return HasRequiredComponents(entity, required);
	}

	protected bool MatchesRelationTarget(IEntitySource entity, RST_WorkbenchSearchEntitiesRequest request, array<string> required)
	{
		return entity && (request.relationClassName.IsEmpty() || entity.GetClassName() == request.relationClassName) && HasRequiredComponents(entity, required);
	}

	protected bool FindRelation(IEntitySource entity, RST_WorkbenchSearchEntitiesRequest request, array<string> required, out IEntitySource related, out int depth, out bool truncated)
	{
		if (request.relationDirection == "parent" || request.relationDirection == "ancestor")
		{
			BaseContainer parent = entity.GetParent();
			for (depth = 1; parent && depth <= request.relationMaxDepth; depth++)
			{
				IEntitySource candidate = IEntitySource.Cast(parent);
				if (MatchesRelationTarget(candidate, request, required))
				{
					related = candidate;
					return true;
				}
				parent = parent.GetParent();
			}
			return false;
		}
		int visited = 0;
		array<IEntitySource> current = {
			entity
		};
		for (depth = 1; depth <= request.relationMaxDepth; depth++)
		{
			array<IEntitySource> next = {
			};
			foreach (IEntitySource parent : current)
			{
				for (int index, count = parent.GetNumChildren(); index < count; index++)
				{
					IEntitySource candidate = IEntitySource.Cast(parent.GetChild(index));
					if (!candidate)
						continue;
					visited++;
					if (visited > 1024)
					{
						truncated = true;
						return false;
					}
					if (MatchesRelationTarget(candidate, request, required))
					{
						related = candidate;
						return true;
					}
					next.Insert(candidate);
				}
			}
			current = next;
		}
		return false;
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchSearchEntitiesRequest req = RST_WorkbenchSearchEntitiesRequest.Cast(request);
		RST_WorkbenchSearchEntitiesResponse response = new RST_WorkbenchSearchEntitiesResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor || !editor.GetApi())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		bool hasRelation = !req.relationDirection.IsEmpty();
		bool validDirection = req.relationDirection == "parent" || req.relationDirection == "ancestor" || req.relationDirection == "child" || req.relationDirection == "descendant";
		bool invalidRelation = false;
		if (!hasRelation)
			invalidRelation = !req.relationClassName.IsEmpty() || !req.relationComponentClasses.IsEmpty() || req.relationMaxDepth != 0;
		else if (!validDirection || (req.relationClassName.IsEmpty() && req.relationComponentClasses.IsEmpty()) || req.relationMaxDepth < 1 || req.relationMaxDepth > 8)
			invalidRelation = true;
		else if ((req.relationDirection == "parent" || req.relationDirection == "child") && req.relationMaxDepth != 1)
			invalidRelation = true;
		if (req.offset < 0 || req.limit < 1 || req.limit > 100 || invalidRelation)
		{
			response.status = "invalid-request";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		response.status = "available";
		api.GetWorldPath(response.worldPath);
		array<string> required = new array<string>();
		if (!req.componentClasses.IsEmpty())
			req.componentClasses.Split(";", required, true);
		array<string> relationRequired = new array<string>();
		if (!req.relationComponentClasses.IsEmpty())
			req.relationComponentClasses.Split(";", relationRequired, true);
		int matched = 0;
		int named = 0;
		int returned = 0;
		int relationCandidates = 0;
		int entityCount = api.GetEditorEntityCount();
		for (int index; index < entityCount; index++)
		{
			IEntitySource entity = api.GetEditorEntity(index);
			if (MatchesCandidate(entity, req, required))
			{
				string name = entity.GetName();
				string resource = string.Format("%1", entity.GetResourceName());
				string className = entity.GetClassName();
				bool nameMatch = !req.query.IsEmpty() && name.Contains(req.query);
				bool classMatch = !req.query.IsEmpty() && className.Contains(req.query);
				bool resourceTextMatch = !req.query.IsEmpty() && resource.Contains(req.query);
				IEntitySource related;
				int relationDepth;
				bool relationTruncated = false;
				if (hasRelation && relationCandidates >= MAX_RELATION_CANDIDATES)
				{
					response.relationTraversalTruncated = true;
					response.totalMatches = matched;
					response.namedMatches = named;
					return response;
				}
				if (hasRelation)
					relationCandidates++;
				bool relationMatches = !hasRelation || FindRelation(entity, req, relationRequired, related, relationDepth, relationTruncated);
				if (relationTruncated)
				{
					response.relationTraversalTruncated = true;
				}
				if (relationMatches)
				{
					matched = matched + 1;
					if (!name.IsEmpty())
						named = named + 1;
					if (matched > req.offset)
					{
						if (matched > req.offset + req.limit)
						{
							response.hasMore = true;
							response.totalMatches = matched;
							response.namedMatches = named;
							return response;
						}
						string components;
						int componentCount = ComponentCount(entity);
						for (int componentIndex; componentIndex < componentCount; componentIndex++)
						{
							IEntityComponentSource component = ComponentAt(entity, componentIndex);
							if (!component)
								continue;
							if (!components.IsEmpty())
								components += ",";
							components += string.Format("%1", component.GetClassName());
						}
						string matchedComponents;
						foreach (string expected : required)
						{
							if (!matchedComponents.IsEmpty())
								matchedComponents += ",";
							matchedComponents += expected;
						}
						string matches;
						if (nameMatch)
							matches = "name";
						if (classMatch || !req.className.IsEmpty())
						{
							if (!matches.IsEmpty())
								matches += ",";
							matches += "class";
						}
						if (resourceTextMatch || !req.resourceQuery.IsEmpty())
						{
							if (!matches.IsEmpty())
								matches += ",";
							matches += "resource";
						}
						if (!required.IsEmpty())
						{
							if (!matches.IsEmpty())
								matches += ",";
							matches += "components";
						}
						if (hasRelation)
						{
							if (!matches.IsEmpty())
								matches += ",";
							matches += "relation";
						}
						IEntitySource parent = IEntitySource.Cast(entity.GetParent());
						string parentClass;
						if (parent)
							parentClass = parent.GetClassName();
						string relationDirection;
						string relationDepthText;
						string relationId;
						string relationClass;
						string relationSubScene;
						string relationLayer;
						string relationComponents;
						if (hasRelation)
						{
							relationDirection = req.relationDirection;
							relationDepthText = relationDepth.ToString();
							relationId = related.GetID().ToString();
							relationClass = related.GetClassName();
							relationSubScene = related.GetSubScene().ToString();
							relationLayer = related.GetLayerID().ToString();
							foreach (string expected : relationRequired)
							{
								if (!relationComponents.IsEmpty())
									relationComponents += ",";
								relationComponents += expected;
							}
						}
						className = BoundResultField(className);
						resource = BoundResultField(resource);
						name = BoundResultField(name);
						components = BoundResultField(components);
						matchedComponents = BoundResultField(matchedComponents);
						parentClass = BoundResultField(parentClass);
						relationId = BoundResultField(relationId);
						relationClass = BoundResultField(relationClass);
						relationComponents = BoundResultField(relationComponents);
						resource.Replace("|", "/");
						resource.Replace(";", "/");
						name.Replace("|", "/");
						name.Replace(";", "/");
						parentClass.Replace("|", "/");
						parentClass.Replace(";", "/");
						relationId.Replace("|", "/");
						relationId.Replace(";", "/");
						relationClass.Replace("|", "/");
						relationClass.Replace(";", "/");
						string record = string.Format("%1|%2|%3|%4|%5|%6|%7", entity.GetID().ToString(), className, entity.GetSubScene(), entity.GetLayerID(), resource, name, components);
						record += "|" + matches;
						record += "|" + matchedComponents;
						record += "|" + parentClass;
						record += "|" + entity.GetNumChildren();
						record += "|" + relationDirection;
						record += "|" + relationDepthText;
						record += "|" + relationId;
						record += "|" + relationClass;
						record += "|" + relationSubScene;
						record += "|" + relationLayer;
						record += "|" + relationComponents;
						if (response.results.Length() + record.Length() + 1 > MAX_RESULT_CHARACTERS)
						{
							response.hasMore = true;
							response.totalMatches = matched;
							response.namedMatches = named;
							return response;
						}
						if (!response.results.IsEmpty())
							response.results += ";";
						response.results += record;
						returned = returned + 1;
					}
				}
			}
		}
		response.totalMatches = matched;
		response.namedMatches = named;
		return response;
	}
}
#endif
