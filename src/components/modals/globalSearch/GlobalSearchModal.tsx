import { useState, useCallback, useEffect, useRef, useMemo } from "react";
import { api } from "@/services/api";
import { useTranslation } from "react-i18next";
import {
    Search,
    ArrowUp,
    ArrowDown,
    CornerDownLeft,
    X,
    Loader2,
    Filter,
    User,
    Bot,
    MessageSquare,
    Lightbulb,
    Cloud,
} from "lucide-react";
import { Dialog, DialogContent, Input } from "@/components/ui";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Badge } from "@/components/ui/badge";
import { useAppStore } from "@/store/useAppStore";
import type { ClaudeMessage, ClaudeSession, ContentItem } from "@/types";
import { getProviderLabel, hasNonDefaultProvider, getProviderBadgeStyle } from "@/utils/providers";
import { cn } from "@/lib/utils";
import { toast } from "sonner";
import { getEsSettings } from "@/services/esSettings";

type GlobalSearchResult = ClaudeMessage;

type MessageTypeFilter = "all" | "user" | "assistant";
type SearchMode = "smart" | "phrase" | "fuzzy";
type SortBy = "relevance" | "time";

interface GlobalSearchModalProps {
    isOpen: boolean;
    onClose: () => void;
}

const MAX_RESULTS = 100;

export const GlobalSearchModal = ({
    isOpen,
    onClose,
}: GlobalSearchModalProps) => {
    const { t } = useTranslation();
    const [query, setQuery] = useState("");
    const [results, setResults] = useState<GlobalSearchResult[]>([]);
    const [isSearching, setIsSearching] = useState(false);
    const [selectedIndex, setSelectedIndex] = useState(0);
    const [messageTypeFilter, setMessageTypeFilter] = useState<MessageTypeFilter>("all");
    const [searchMode, setSearchMode] = useState<SearchMode>("smart");
    const [sortBy, setSortBy] = useState<SortBy>("relevance");
    const [resultsFromEs, setResultsFromEs] = useState(false);
    // Whether ES is configured. ES-only UI affordances (mode toggle, sort
    // toggle, score chip) are hidden when not enabled to keep the search
    // experience identical to vanilla CCHV for users who haven't opted in.
    const [esEnabled, setEsEnabled] = useState(false);
    const inputRef = useRef<HTMLInputElement>(null);
    const resultsContainerRef = useRef<HTMLDivElement>(null);
    const debounceTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);

    const { claudePath, projects, selectProject, selectSession, sessions, getSessionDisplayName, activeProviders, navigateToMessage, clearTargetMessage, setAnalyticsCurrentView, userMetadata } =
        useAppStore();
    const [selectedProjectPath, setSelectedProjectPath] = useState<string>("all");

    // Group results by project name
    const groupedResults = useMemo(() => {
        const groups = new Map<string, { label: string; provider?: string; items: GlobalSearchResult[] }>();

        for (const result of results) {
            const projectName =
                result.projectName || t("globalSearch.unknownProject");
            const providerLabel = getProviderLabel(
                (key, fallback) => t(key, fallback),
                result.provider,
            );
            const groupKey = `${result.provider ?? "claude"}::${projectName}`;
            const groupLabel = `${projectName} (${providerLabel})`;

            if (!groups.has(groupKey)) {
                groups.set(groupKey, { label: groupLabel, provider: result.provider, items: [] });
            }
            groups.get(groupKey)!.items.push(result);
        }

        return groups;
    }, [results, t]);

    // Flatten grouped results for keyboard navigation
    const flattenedResults = useMemo(() => {
        const flat: GlobalSearchResult[] = [];
        for (const group of groupedResults.values()) {
            flat.push(...group.items);
        }
        return flat;
    }, [groupedResults]);

    // Get session display name for a search result
    const getSessionName = useCallback((result: GlobalSearchResult): string | undefined => {
        if (!result.sessionId) return undefined;
        return getSessionDisplayName(result.sessionId);
    }, [getSessionDisplayName]);

    // Debounced search
    const performSearch = useCallback(
        async (searchQuery: string) => {
            const trimmedQuery = searchQuery.trim();

            if (!claudePath || trimmedQuery.length < 2) {
                setResults([]);
                setIsSearching(false);
                return;
            }

            setIsSearching(true);
            try {
                const filters: Record<string, unknown> = {};
                if (selectedProjectPath !== "all") {
                    // Backend matches by directory name (last path segment), not full path
                    const dirName = selectedProjectPath.split(/[\\/]/).pop() || selectedProjectPath;
                    filters.projects = [dirName];
                }
                if (messageTypeFilter !== "all") {
                    filters.messageType = messageTypeFilter;
                }
                // ES-only filters (silently ignored by local backends)
                filters.searchMode = searchMode;
                filters.sortBy = sortBy;

                // Try ES search first (if configured)
                let searchResults: GlobalSearchResult[] | null = null;
                let usedEs = false;
                try {
                    const esSettings = await getEsSettings();
                    if (esSettings?.endpoint) {
                        searchResults = await api<GlobalSearchResult[]>(
                            "es_search_messages",
                            {
                                endpoint: esSettings.endpoint,
                                username: esSettings.username,
                                password: esSettings.password,
                                query: trimmedQuery,
                                filters,
                                limit: MAX_RESULTS,
                            },
                        );
                        usedEs = true;
                    }
                } catch {
                    // ES unavailable, fall through to local search
                    searchResults = null;
                    usedEs = false;
                }

                // Fallback to local search if ES didn't return results
                if (searchResults === null) {
                    const hasNonClaudeProviders = hasNonDefaultProvider(activeProviders);
                    const wslEnabled = userMetadata?.settings?.wsl?.enabled ?? false;
                    const wslExcludedDistros = userMetadata?.settings?.wsl?.excludedDistros ?? [];
                    searchResults = await api<GlobalSearchResult[]>(
                        (hasNonClaudeProviders || wslEnabled) ? "search_all_providers" : "search_messages",
                        (hasNonClaudeProviders || wslEnabled)
                            ? { claudePath, query: trimmedQuery, activeProviders, filters, limit: MAX_RESULTS, wslEnabled, wslExcludedDistros }
                            : { claudePath, query: trimmedQuery, filters, limit: MAX_RESULTS },
                    );
                    usedEs = false;
                }

                setResults(searchResults);
                setResultsFromEs(usedEs);
                setSelectedIndex(0);
            } catch (error) {
                console.error("Global search failed:", error);
                setResults([]);
                toast.error(t("globalSearch.searchFailed"));
            } finally {
                setIsSearching(false);
            }
        },
        [claudePath, activeProviders, selectedProjectPath, messageTypeFilter, searchMode, sortBy, userMetadata, t],
    );

    // Handle input change with debounce
    const handleInputChange = useCallback(
        (e: React.ChangeEvent<HTMLInputElement>) => {
            const value = e.target.value;
            setQuery(value);

            if (debounceTimeoutRef.current) {
                clearTimeout(debounceTimeoutRef.current);
            }

            debounceTimeoutRef.current = setTimeout(() => {
                performSearch(value);
            }, 300);
        },
        [performSearch],
    );

    // Navigate to selected result
    const handleSelectResult = useCallback(
        async (result: GlobalSearchResult) => {
            try {
                let targetSession = sessions.find(
                    (s) =>
                        s.session_id === result.sessionId ||
                        s.actual_session_id === result.sessionId,
                );

                if (targetSession) {
                    // Ensure the conversation pane is the active view — otherwise
                    // a result clicked while in analytics/tokenStats/etc. loads the
                    // session but stays hidden behind the other view (issue #390).
                    setAnalyticsCurrentView("messages");
                    if (result.uuid) navigateToMessage(result.uuid);
                    await selectSession(targetSession);
                    onClose();
                    return;
                }

                // Snapshot excludeSidechain once to keep requests consistent
                // across the scan and avoid repeated getState() calls. The
                // setting is user-configurable; taking a snapshot is intentional
                // so a mid-scan toggle does not change half the requests.
                const { excludeSidechain } = useAppStore.getState();

                // Try the result's own `projectName` first. ES results carry
                // the raw encoded project directory name (e.g.
                // "Users-rassyan-WebstormProjects-claude-code-history-viewer")
                // which equals each project's `path.basename`, so that
                // matches every provider that uses dash-encoded directories
                // (Claude / CodeBuddy). Falling back to a full scan would
                // load every project's session list serially, which is the
                // CMD+K "long pause then nothing happens" behavior users
                // see when there are many projects.
                const projectsToTry = (() => {
                    if (!result.projectName) return projects;
                    const direct = projects.find((p) => {
                        if (p.name === result.projectName) return true;
                        // Match by trailing path segment so the dash-encoded
                        // directory name lines up with the displayed project.
                        const base = p.path.split(/[\\/]/).pop();
                        return base === result.projectName;
                    });
                    if (!direct) return projects;
                    // Tried first; rest are fallbacks if dispatch fails.
                    return [direct, ...projects.filter((p) => p !== direct)];
                })();

                for (const project of projectsToTry) {
                    try {
                        const projectProvider = project.provider ?? "claude";
                        const projectSessions = await api<ClaudeSession[]>(
                            projectProvider !== "claude" ? "load_provider_sessions" : "load_project_sessions",
                            projectProvider !== "claude"
                                ? { provider: projectProvider, projectPath: project.path, excludeSidechain }
                                : { projectPath: project.path, excludeSidechain },
                        );

                        targetSession = projectSessions.find(
                            (s) =>
                                s.session_id === result.sessionId ||
                                s.actual_session_id === result.sessionId,
                        );

                        if (targetSession) {
                            setAnalyticsCurrentView("messages");
                            if (result.uuid) navigateToMessage(result.uuid);
                            await selectProject(project);
                            await selectSession(targetSession);
                            onClose();
                            return;
                        }
                    } catch (error) {
                        console.error(
                            `Failed to load sessions for project ${project.name}:`,
                            error,
                        );
                    }
                }

                // Session not found in any local project — try ES cloud fallback
                const esSettings = await getEsSettings();
                if (esSettings?.endpoint && result.sessionId) {
                    // For subagent paths (.../subagents/agent-xxx.jsonl), the session
                    // UUID lives in the parent directory, not the filename.
                    // Standard paths use the filename minus .jsonl.
                    const extractActualSessionId = (path: string): string => {
                        const parts = path.split("/");
                        const fileName = parts.pop() ?? path;
                        if (parts[parts.length - 1] === "subagents" && parts.length >= 2) {
                            // Use the parent of "subagents" as the session UUID
                            return parts[parts.length - 2] ?? fileName.replace(/\.jsonl$/, "");
                        }
                        return fileName.replace(/\.jsonl$/, "");
                    };

                    const cloudSession: ClaudeSession = {
                        session_id: result.sessionId,
                        actual_session_id: extractActualSessionId(result.sessionId),
                        file_path: result.sessionId,
                        project_name: result.projectName || "Cloud",
                        message_count: 0,
                        first_message_time: result.timestamp || "",
                        last_message_time: result.timestamp || "",
                        last_modified: result.timestamp || "",
                        has_tool_use: false,
                        has_errors: false,
                        summary: undefined,
                        provider: result.provider as ClaudeSession["provider"],
                        storage_type: "elasticsearch",
                    };
                    if (result.uuid) navigateToMessage(result.uuid);
                    await selectSession(cloudSession);
                    onClose();
                    return;
                }

                clearTargetMessage();
                toast.error(t("globalSearch.sessionNotFound"));
                onClose();
            } catch (error) {
                clearTargetMessage();
                console.error("Failed to navigate to search result:", error);
                toast.error(t("globalSearch.navigationFailed"));
                onClose();
            }
        },
        [projects, sessions, selectProject, selectSession, navigateToMessage, clearTargetMessage, setAnalyticsCurrentView, onClose, t],
    );

    // Keyboard navigation
    const handleKeyDown = useCallback(
        (e: React.KeyboardEvent) => {
            if (flattenedResults.length === 0) return;

            switch (e.key) {
                case "ArrowDown":
                    e.preventDefault();
                    setSelectedIndex((prev) =>
                        prev < flattenedResults.length - 1 ? prev + 1 : 0,
                    );
                    break;
                case "ArrowUp":
                    e.preventDefault();
                    setSelectedIndex((prev) =>
                        prev > 0 ? prev - 1 : flattenedResults.length - 1,
                    );
                    break;
                case "Enter":
                    e.preventDefault();
                    if (flattenedResults[selectedIndex]) {
                        handleSelectResult(flattenedResults[selectedIndex]);
                    }
                    break;
                case "Escape":
                    e.preventDefault();
                    onClose();
                    break;
            }
        },
        [flattenedResults, selectedIndex, handleSelectResult, onClose],
    );

    // Scroll selected item into view
    useEffect(() => {
        if (resultsContainerRef.current && flattenedResults.length > 0) {
            const selectedElement = resultsContainerRef.current.querySelector(
                `[data-index="${selectedIndex}"]`,
            );
            selectedElement?.scrollIntoView({ block: "nearest" });
        }
    }, [selectedIndex, flattenedResults.length]);

    // Focus input when modal opens
    useEffect(() => {
        if (isOpen) {
            setTimeout(() => inputRef.current?.focus(), 0);
        } else {
            setQuery("");
            setResults([]);
            setSelectedIndex(0);
            setSelectedProjectPath("all");
            setMessageTypeFilter("all");
        }
    }, [isOpen]);

    // Detect whether ES is configured so we can hide ES-only UI when off.
    useEffect(() => {
        if (!isOpen) return;
        let cancelled = false;
        (async () => {
            const settings = await getEsSettings();
            if (!cancelled) setEsEnabled(!!settings?.endpoint);
        })();
        return () => {
            cancelled = true;
        };
    }, [isOpen]);

    // Re-search when filters change. `query` is intentionally omitted —
    // keystroke-driven searches go through handleInputChange's debounce.
    // This effect only fires when performSearch identity changes (i.e., filter deps).
    useEffect(() => {
        if (query.trim().length >= 2) {
            performSearch(query);
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [performSearch]);

    // Cleanup debounce on unmount
    useEffect(() => {
        return () => {
            if (debounceTimeoutRef.current) {
                clearTimeout(debounceTimeoutRef.current);
            }
        };
    }, []);

    // Get preview text centered around the search term
    const getPreviewText = (message: GlobalSearchResult): string => {
        if (!message.content) return t("globalSearch.noPreview");

        const content = message.content;
        let fullText = "";

        if (typeof content === "string") {
            fullText = content;
        } else if (Array.isArray(content)) {
            const texts: string[] = [];
            for (const item of content as ContentItem[]) {
                if (item.type === "text" && "text" in item) {
                    texts.push(item.text as string);
                }
            }
            fullText = texts.join(" ");
        }

        if (!fullText) return t("globalSearch.noPreview");

        // Find search term position and show surrounding context
        const trimmedQuery = query.trim().toLowerCase();
        if (trimmedQuery.length >= 2) {
            const lowerText = fullText.toLowerCase();
            const matchIndex = lowerText.indexOf(trimmedQuery);
            if (matchIndex !== -1) {
                const contextRadius = 60;
                const start = Math.max(0, matchIndex - contextRadius);
                const end = Math.min(fullText.length, matchIndex + trimmedQuery.length + contextRadius);
                const slice = fullText.slice(start, end);
                const prefix = start > 0 ? "..." : "";
                const suffix = end < fullText.length ? "..." : "";
                return prefix + slice + suffix;
            }
        }

        return fullText.slice(0, 150) + (fullText.length > 150 ? "..." : "");
    };

    // Render ES-provided highlight HTML safely.
    //
    // The backend sets `searchPreviewHtml` from ES highlight fragments containing
    // *only* `<mark>...</mark>` tags (no user-controlled HTML — ES escapes the
    // source text, then re-injects the configured `pre_tags`/`post_tags`).
    // We still escape any other characters as a defense in depth and only allow
    // `<mark>` / `</mark>` to be rendered as actual tags.
    const renderEsHighlight = (html: string): React.ReactNode => {
        // Split on `<mark>...</mark>` while preserving the matched parts
        const parts = html.split(/(<mark>.*?<\/mark>)/g);
        return parts.map((part, idx) => {
            const m = /^<mark>(.*?)<\/mark>$/.exec(part);
            if (m) {
                return (
                    <mark
                        key={idx}
                        className="bg-yellow-300 dark:bg-yellow-500/40 text-foreground rounded-sm px-0.5"
                    >
                        {m[1]}
                    </mark>
                );
            }
            return <span key={idx}>{part}</span>;
        });
    };

    // Format timestamp
    const formatTimestamp = (timestamp: string): string => {
        try {
            const date = new Date(timestamp);
            return date.toLocaleDateString(undefined, {
                month: "short",
                day: "numeric",
                hour: "2-digit",
                minute: "2-digit",
            });
        } catch {
            return "";
        }
    };

    // Memoize regex to avoid re-creation per result item
    const highlightRegex = useMemo(() => {
        const trimmed = query.trim();
        if (!trimmed) return null;
        return new RegExp(
            `(${trimmed.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")})`,
            "i",
        );
    }, [query]);

    const highlightText = (text: string): React.ReactNode => {
        if (!highlightRegex) return text;

        const parts = text.split(highlightRegex);
        return parts.map((part, index) =>
            highlightRegex.test(part) ? (
                <mark
                    key={index}
                    className="bg-yellow-300 dark:bg-yellow-500/40 text-foreground rounded-sm px-0.5"
                >
                    {part}
                </mark>
            ) : (
                part
            ),
        );
    };

    let currentResultIndex = 0;

    return (
        <Dialog open={isOpen} onOpenChange={(open) => !open && onClose()}>
            <DialogContent
                className="sm:max-w-2xl p-0 gap-0 overflow-hidden"
                onKeyDown={handleKeyDown}
                showCloseButton={false}
                aria-label={t("globalSearch.title")}
            >
                {/* Search Header */}
                <div className="flex items-center gap-3 px-4 py-3 border-b border-border">
                    <Search className="w-4 h-4 text-muted-foreground shrink-0" />
                    <Input
                        ref={inputRef}
                        type="text"
                        value={query}
                        onChange={handleInputChange}
                        placeholder={t("globalSearch.placeholder")}
                        className="border-0 shadow-none focus-visible:ring-0 px-0 h-auto text-sm"
                        autoComplete="off"
                        autoCorrect="off"
                        autoCapitalize="off"
                        spellCheck={false}
                    />
                    {isSearching && (
                        <Loader2 className="w-4 h-4 text-muted-foreground animate-spin shrink-0" />
                    )}
                    {query && !isSearching && (
                        <button
                            onClick={() => {
                                setQuery("");
                                setResults([]);
                                inputRef.current?.focus();
                            }}
                            className="p-1 hover:bg-muted rounded"
                            aria-label={t("globalSearch.clearSearch")}
                        >
                            <X className="w-3 h-3 text-muted-foreground" />
                        </button>
                    )}
                </div>

                {/* Filters Bar */}
                <div className="flex items-center gap-2 px-4 py-2 border-b border-border bg-muted/20 flex-wrap">
                    {/* Message Type Filter */}
                    <div className="flex items-center gap-1">
                        {(["all", "user", "assistant"] as const).map((type) => (
                            <button
                                key={type}
                                onClick={() => setMessageTypeFilter(type)}
                                className={cn(
                                    "flex items-center gap-1 px-2 py-1 text-xs rounded-md transition-colors",
                                    messageTypeFilter === type
                                        ? "bg-foreground/10 text-foreground font-medium"
                                        : "text-muted-foreground hover:text-foreground hover:bg-muted"
                                )}
                                aria-label={t(`globalSearch.filterType.${type}`)}
                            >
                                {type === "all" && <MessageSquare className="w-3 h-3" />}
                                {type === "user" && <User className="w-3 h-3" />}
                                {type === "assistant" && <Bot className="w-3 h-3" />}
                                <span>{t(`globalSearch.filterType.${type}`)}</span>
                            </button>
                        ))}
                    </div>

                    {/* ES-only controls — hidden when ES is not configured to keep
                        the search UI identical to vanilla CCHV for non-users. */}
                    {esEnabled && (
                        <>
                            <div className="w-px h-4 bg-border" />

                            {/* Search Mode */}
                            <div className="flex items-center gap-1">
                                {([
                                    { v: "smart", label: t("globalSearch.mode.smart", { defaultValue: "Smart" }), title: t("globalSearch.mode.smartHint", { defaultValue: "Tokenized OR matching, ranks by relevance" }) },
                                    { v: "phrase", label: t("globalSearch.mode.phrase", { defaultValue: "Phrase" }), title: t("globalSearch.mode.phraseHint", { defaultValue: "Match exact phrase (after tokenization)" }) },
                                    { v: "fuzzy", label: t("globalSearch.mode.fuzzy", { defaultValue: "Fuzzy" }), title: t("globalSearch.mode.fuzzyHint", { defaultValue: "Tolerate typos (more noise)" }) },
                                ] as const).map(({ v, label, title }) => (
                                    <button
                                        key={v}
                                        onClick={() => setSearchMode(v)}
                                        title={title}
                                        className={cn(
                                            "px-2 py-1 text-xs rounded-md transition-colors",
                                            searchMode === v
                                                ? "bg-foreground/10 text-foreground font-medium"
                                                : "text-muted-foreground hover:text-foreground hover:bg-muted"
                                        )}
                                    >
                                        {label}
                                    </button>
                                ))}
                            </div>

                            <div className="w-px h-4 bg-border" />

                            {/* Sort by */}
                            <div className="flex items-center gap-1">
                                {([
                                    { v: "relevance", label: t("globalSearch.sort.relevance", { defaultValue: "Relevance" }) },
                                    { v: "time", label: t("globalSearch.sort.time", { defaultValue: "Newest" }) },
                                ] as const).map(({ v, label }) => (
                                    <button
                                        key={v}
                                        onClick={() => setSortBy(v)}
                                        className={cn(
                                            "px-2 py-1 text-xs rounded-md transition-colors",
                                            sortBy === v
                                                ? "bg-foreground/10 text-foreground font-medium"
                                                : "text-muted-foreground hover:text-foreground hover:bg-muted"
                                        )}
                                    >
                                        {label}
                                    </button>
                                ))}
                            </div>
                        </>
                    )}

                    {/* Divider */}
                    {projects.length > 1 && (
                        <div className="w-px h-4 bg-border" />
                    )}

                    {/* Project Filter */}
                    {projects.length > 1 && (
                        <>
                            <Filter className="w-3.5 h-3.5 text-muted-foreground shrink-0" />
                            <Select value={selectedProjectPath} onValueChange={setSelectedProjectPath}>
                                <SelectTrigger className="h-7 text-xs border-border w-40">
                                    <SelectValue placeholder={t("globalSearch.allProjects")} />
                                </SelectTrigger>
                                <SelectContent>
                                    <SelectItem value="all">{t("globalSearch.allProjects")}</SelectItem>
                                    {projects.map((project) => (
                                        <SelectItem key={project.path} value={project.path}>
                                            {project.name}
                                        </SelectItem>
                                    ))}
                                </SelectContent>
                            </Select>
                        </>
                    )}
                </div>

                {/* Results */}
                <div
                    ref={resultsContainerRef}
                    className="max-h-100 overflow-y-auto"
                >
                    {/* Loading skeleton */}
                    {isSearching && results.length === 0 && (
                        <div className="py-4 space-y-3 px-4">
                            {Array.from({ length: 4 }).map((_, i) => (
                                <div key={i} className="animate-pulse">
                                    <div className="flex items-center gap-2 mb-1.5">
                                        <div className="h-4 w-12 bg-muted rounded" />
                                        <div className="h-3 w-20 bg-muted rounded" />
                                    </div>
                                    <div className="h-4 w-full bg-muted rounded mb-1" />
                                    <div className="h-4 w-3/4 bg-muted rounded" />
                                </div>
                            ))}
                        </div>
                    )}

                    {!isSearching && query.trim().length >= 2 && results.length === 0 && (
                        <div className="px-4 py-8 text-center text-sm text-muted-foreground">
                            {t("globalSearch.noResults")}
                        </div>
                    )}

                    {/* Empty state with search tips */}
                    {!query && (
                        <div className="px-6 py-8 space-y-4">
                            <div className="text-center">
                                <Search className="w-8 h-8 text-muted-foreground/40 mx-auto mb-3" />
                                <p className="text-sm text-muted-foreground">
                                    {t("globalSearch.hint")}
                                </p>
                            </div>
                            <div className="space-y-2">
                                <div className="flex items-start gap-2 text-xs text-muted-foreground/70">
                                    <Lightbulb className="w-3.5 h-3.5 mt-0.5 shrink-0" />
                                    <span>{t("globalSearch.tips.minChars")}</span>
                                </div>
                                <div className="flex items-start gap-2 text-xs text-muted-foreground/70">
                                    <Lightbulb className="w-3.5 h-3.5 mt-0.5 shrink-0" />
                                    <span>{t("globalSearch.tips.filters")}</span>
                                </div>
                                <div className="flex items-start gap-2 text-xs text-muted-foreground/70">
                                    <Lightbulb className="w-3.5 h-3.5 mt-0.5 shrink-0" />
                                    <span>{t("globalSearch.tips.navigate")}</span>
                                </div>
                            </div>
                        </div>
                    )}

                    {/* Typing but not enough chars */}
                    {query && query.trim().length < 2 && !isSearching && (
                        <div className="px-4 py-8 text-center text-sm text-muted-foreground">
                            {t("globalSearch.tips.minChars")}
                        </div>
                    )}

                    {results.length > 0 && (
                        <div className="py-2">
                            {/* ES data source indicator */}
                            {resultsFromEs && (
                                <div className="px-4 py-1 text-xs text-sky-600 dark:text-sky-400 flex items-center gap-1.5 border-b border-sky-500/20 bg-sky-500/5">
                                    <Cloud className="w-3 h-3" />
                                    <span>{t("globalSearch.resultsFromEs", "Results from Elasticsearch")}</span>
                                </div>
                            )}
                            {Array.from(groupedResults.entries()).map(
                                ([groupKey, group]) => (
                                    <div key={groupKey}>
                                        {/* Project Header */}
                                        <div className="px-4 py-1.5 text-xs font-medium text-muted-foreground bg-muted sticky top-0 truncate flex items-center gap-2">
                                            {group.provider && group.provider !== "claude" && (
                                                <Badge
                                                    size="sm"
                                                    className={cn(
                                                        "rounded px-1 py-0 text-2xs",
                                                        getProviderBadgeStyle(group.provider)
                                                    )}
                                                >
                                                    {getProviderLabel((key, fallback) => t(key, fallback), group.provider)}
                                                </Badge>
                                            )}
                                            <span className="truncate">{group.label}</span>
                                        </div>

                                        {/* Results in this project */}
                                        {group.items.map((result) => {
                                            const index = currentResultIndex++;
                                            const isSelected = index === selectedIndex;

                                            return (
                                                <button
                                                    key={result.uuid}
                                                    data-index={index}
                                                    onClick={() => handleSelectResult(result)}
                                                    className={cn(
                                                        "w-full text-left px-4 py-2.5 hover:bg-muted/50 transition-colors",
                                                        isSelected && "bg-muted"
                                                    )}
                                                >
                                                    <div className="flex items-start gap-3">
                                                        <div className="flex-1 min-w-0">
                                                            <div className="flex items-center gap-2 mb-1">
                                                                <span
                                                                    className={cn(
                                                                        "inline-flex items-center gap-1 text-xs px-1.5 py-0.5 rounded font-medium",
                                                                        result.type === "user"
                                                                            ? "bg-blue-500/10 text-blue-500"
                                                                            : result.type === "assistant"
                                                                              ? "bg-amber-500/10 text-amber-500"
                                                                              : "bg-gray-500/10 text-gray-500"
                                                                    )}
                                                                >
                                                                    {result.type === "user" && <User className="w-3 h-3" />}
                                                                    {result.type === "assistant" && <Bot className="w-3 h-3" />}
                                                                    {result.type}
                                                                </span>
                                                                <span className="text-xs text-muted-foreground">
                                                                    {formatTimestamp(result.timestamp)}
                                                                </span>
                                                                {typeof result.searchScore === "number" && (
                                                                    <span
                                                                        className="text-2xs text-muted-foreground/60 font-mono"
                                                                        title={t("globalSearch.relevanceScoreHint", { defaultValue: "Elasticsearch BM25 score (higher = more relevant)" })}
                                                                    >
                                                                        {result.searchScore.toFixed(1)}
                                                                    </span>
                                                                )}
                                                            </div>
                                                            {(() => {
                                                                const sessionName = getSessionName(result);
                                                                return sessionName ? (
                                                                    <p className="text-xs text-muted-foreground/70 truncate mb-0.5">
                                                                        {sessionName}
                                                                    </p>
                                                                ) : null;
                                                            })()}
                                                            <p className="text-sm text-foreground line-clamp-2">
                                                                {result.searchPreviewHtml
                                                                    ? renderEsHighlight(result.searchPreviewHtml)
                                                                    : highlightText(getPreviewText(result))}
                                                            </p>
                                                        </div>
                                                    </div>
                                                </button>
                                            );
                                        })}
                                    </div>
                                ),
                            )}
                        </div>
                    )}
                </div>

                {/* Footer with keyboard hints */}
                <div className="flex items-center justify-between px-4 py-2 border-t border-border bg-muted/30 text-xs text-muted-foreground">
                    <div className="flex items-center gap-4">
                        <div className="flex items-center gap-1">
                            <kbd className="px-1.5 py-0.5 bg-muted rounded border border-border font-mono">
                                <ArrowUp className="w-3 h-3 inline" />
                            </kbd>
                            <kbd className="px-1.5 py-0.5 bg-muted rounded border border-border font-mono">
                                <ArrowDown className="w-3 h-3 inline" />
                            </kbd>
                            <span className="ml-1">
                                {t("globalSearch.navigate")}
                            </span>
                        </div>
                        <div className="flex items-center gap-1">
                            <kbd className="px-1.5 py-0.5 bg-muted rounded border border-border font-mono">
                                <CornerDownLeft className="w-3 h-3 inline" />
                            </kbd>
                            <span className="ml-1">
                                {t("globalSearch.select")}
                            </span>
                        </div>
                        <div className="flex items-center gap-1">
                            <kbd className="px-1.5 py-0.5 bg-muted rounded border border-border font-mono text-px10">
                                esc
                            </kbd>
                            <span className="ml-1">
                                {t("globalSearch.close")}
                            </span>
                        </div>
                    </div>
                    {results.length > 0 && (
                        <span>
                            {t("globalSearch.results", {
                                count: results.length,
                            })}
                        </span>
                    )}
                </div>
            </DialogContent>
        </Dialog>
    );
};
